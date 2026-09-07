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
> and **do not add `--bypassdedicateduser`**. On Linux the server refuses to boot
> under a login account and prints how your human provisions the dedicated
> `mwe-mcp` user — **that refusal is correct, not an error to work around.** Stop
> and let the human run that step. On macOS and Windows nothing refuses: the
> server starts as whoever you are, and the boundary it would have enforced is
> still needed, so stop there too and hand your human
> [Run it as a service](#run-it-as-a-service).

---

## Supported platforms

Three, and they are all the gate: **Linux x86_64**, **macOS on Apple
Silicon**, and **Windows x86_64**. Every push to `main`, and every pull
request against it, runs the whole test suite on all three — no test is
skipped on any of them — and a release publishes a prebuilt binary for each.
On anything else — an Intel Mac, ARM Linux — you build from source, which
works but is not something CI watches.

What is verified is not the same on all three, so here is the split:

| | Linux x86_64 | macOS (Apple Silicon) | Windows x86_64 |
|---|---|---|---|
| **CI: the full test suite, nothing skipped** | yes | yes | yes |
| **A release publishes a prebuilt binary** | yes | yes | yes |
| **Run as a service, restarting on boot** | the server sets it up for you (systemd) | you install the launchd plist ([Run it as a service](#run-it-as-a-service)) | you register the scheduled task ([Run it as a service](#run-it-as-a-service)) |
| **`serve` refuses to start under your login account** | yes | no — the check reads Linux-only files | no |
| **`mwe-mcp doctor` audits the workdir permissions** | yes | yes | no — it reads POSIX mode bits, and Windows uses ACLs |
| **Desktop tray icon** | Linux only (see below) | — | — |

Read the middle three rows together, because they are one thing: **the daemon
belongs to an account nobody logs in with**, and on Linux the server insists
on that and provisions it for you, while on macOS and Windows it is yours to
set up and nothing stops you skipping it. The reason it matters is the same
everywhere — per-reader redaction is applied when the server *renders* an
answer, and the memory under the workdir is cleartext on disk, so anything
running as an account that can read those files reads every fragment
un-redacted. [Run it as a service](#run-it-as-a-service) is that setup, per
platform.

**The tray icon is Linux only.** `mwe-mcp-tray` is an optional desktop
control surface — an icon that says whether the service is running, and a
menu to start, stop and restart it. It is a separate binary that no release
publishes: it talks to systemd, and it draws itself through the D-Bus status
notifier protocol that KDE and GNOME implement and macOS and Windows do not.
It controls nothing the dashboard and your platform's own service tools do
not, so its absence costs you a convenience, not a capability.

---

## 1. Get the binary

**Linux / macOS** — download-and-run:

```bash
curl -fsSL https://raw.githubusercontent.com/Fr4nZ82/mwe-mcp/main/install.sh | sh
```

This fetches the right prebuilt binary for your OS/arch from the
[Releases](https://github.com/Fr4nZ82/mwe-mcp/releases), verifies its SHA-256, and
installs it to `~/.local/bin/mwe-mcp`. Override the install dir with
`MWE_MCP_BINDIR` or pin a version with `MWE_MCP_VERSION` (default: latest).

**Windows** — open the [latest release](https://github.com/Fr4nZ82/mwe-mcp/releases/latest),
download `mwe-mcp-<version>-x86_64-pc-windows-msvc.zip` from its **Assets**
section, and unzip it. The
binary is unsigned, so SmartScreen may warn the first time you run it — pick
"More info → Run anyway", or check the download against its published `.sha256`
first if you want the reassurance.

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

  On Linux, run interactively, it **offers to install a restart-on-boot
  systemd service**; on macOS and Windows you set that up yourself, in
  [Run it as a service](#run-it-as-a-service). Put TLS in front (a reverse
  proxy or a Cloudflare Tunnel) — the endpoint is JWT-gated but plain HTTP —
  and mint `exposed` (30-day) tokens for remote consumers.
- **Same machine as a consumer agent** (e.g. Hermes) — the boundary is real, so run

  ```bash
  mwe-mcp serve --bind 127.0.0.1
  ```

  **with no `--bypassdedicateduser`** — passing it would disable exactly the
  boundary you need here. On Linux `serve` **won't start under your login
  account**: from an interactive terminal it **offers to provision a dedicated
  `mwe-mcp` user**, lock the workdir, and install the systemd service for you.
  Let that prompt run; the local consumer then connects to `127.0.0.1`. On
  macOS and Windows nothing refuses, so this is the case where you must do it
  yourself — [Run it as a service](#run-it-as-a-service) is the walkthrough.

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
   internal models classify every turn, walk the pages at recall, deduplicate,
   run the nightly REM cycle, write the prose and drive the dashboard chat —
   six roles, and the product does not work until all six have a model). Set a provider's API key — Anthropic, Google Gemini, or
   OpenRouter — or point a role at a local [Ollama](https://ollama.com) model,
   then assign each role. A quick profile fills every role in one click:

   | Preset | Routing | Needs |
   |---|---|---|
   | **`all-api`** | every generative function on an external provider (Anthropic / Gemini) | API keys; no local model |
   | **`hybrid`** | `ingest` and `operator_chat` on a local Ollama model; the navigator, `cronista` and the nightly REM roles on an API model | a local Ollama workhorse + API keys |
   | **`all-local`** | local workhorse for everything (Ollama + Qwen/Llama) | strong local hardware (a GPU); zero API cost, fully offline |
   | **`custom`** | wire nothing up front, pick every role from the dashboard | — |

   **Embeddings always run locally and are free** — independent of this choice.
   Already running Ollama with an embedder? Switch the embedding backend to
   `ollama` from **Admin → Embedding** to avoid keeping a second model. That is
   the one embedder that leaves the process, so its calls are counted on the
   Usage & spend page; the bundled one runs inside the binary and is not.

   > **How capable does the internal LLM need to be?** The `ingest` role is a
   > structured router: it must emit valid plans with exact wiki ids, every
   > turn. In our testing, **small local models (≤ ~10B) route unreliably** —
   > they hallucinate target ids and facts get dropped — so `all-local` wants
   > a genuinely strong local model, and `hybrid` (local `ingest` and chat,
   > an API model on the navigator, `cronista` and REM) is the safer budget
   > setup; a strong API model on `ingest` alone is the safer one still. If pages
   > come out empty or badly filed, suspect the model before the engine.
3. **Do the short profile primer** the wizard shows next (your name, language, a
   few preferences) so the memory starts with some context — or skip it.
4. **Mint a token for your agent** (Admin → users / tokens). User ids are
   plain lowercase letters and digits (`anna`, `sam2`); the enrollment form
   refuses anything else, because the id is also the name of the person's
   identity wiki.

   > **Connecting Claude Code?** Skip the token: it signs in over OAuth instead
   > (see [Next: connect an agent](#next-connect-an-agent)). You still need the
   > user created here.

That's it — you have a running, governed memory.

---

## Run it as a service

A server you have to remember to start is a memory that is missing whenever
you forgot. This section makes `mwe-mcp` a background daemon that comes back
after a reboot and restarts if it crashes — and, just as important, one that
runs as **an account nobody logs in with**.

That account is the point of the exercise. The per-reader redaction is
applied when the server renders an answer, but `wikis/` and `engine.db` under
the workdir are plain files. Any process running as an account that can read
them reads the un-redacted union of every fragment, whoever it was about — a
co-located agent with file tools included. Giving the daemon its own account,
and the workdir to that account alone, is what keeps them out.

### Linux — the server does it for you

Run `mwe-mcp serve` in a terminal. It refuses to start under your login
account and offers to do the whole thing: create the `mwe-mcp` system
account, move the workdir to `/home/mwe-mcp/workdir` and lock it to `700`,
install the binary to `/usr/local/bin/mwe-mcp`, write the systemd unit and
start it. Say yes and it hands the port to the service before it exits.

```bash
sudo systemctl status mwe-mcp     # is it running?
sudo systemctl restart mwe-mcp    # after a binary swap
journalctl -u mwe-mcp -f          # follow the logs
```

There is nothing to copy from this repository: the unit is rendered by the
binary itself, so it always agrees with the binary that will run it.

### macOS — a launchd daemon

The plist is [`packaging/macos/com.mwe-mcp.server.plist`](packaging/macos/com.mwe-mcp.server.plist).
Provision the account and the directories it names, then load it:

```bash
# 1. A hidden role account with no login shell. macOS names these with a
#    leading underscore and hides UIDs under 500; check 401 is free first:
#      dscl . -list /Users UniqueID | awk '{print $2}' | sort -n | tail -20
sudo dscl . -create /Groups/_mwe-mcp PrimaryGroupID 401
sudo dscl . -create /Users/_mwe-mcp UserShell /usr/bin/false
sudo dscl . -create /Users/_mwe-mcp UniqueID 401
sudo dscl . -create /Users/_mwe-mcp PrimaryGroupID 401
sudo dscl . -create /Users/_mwe-mcp NFSHomeDirectory /usr/local/var/mwe-mcp

# 2. The binary where the daemon can exec it, and its own tree, owner-only.
sudo install -m 755 ./mwe-mcp /usr/local/bin/mwe-mcp
sudo mkdir -p /usr/local/var/mwe-mcp/{workdir,cache,logs}
sudo chown -R _mwe-mcp:_mwe-mcp /usr/local/var/mwe-mcp
sudo chmod -R go-rwx /usr/local/var/mwe-mcp

# 3. The daemon. launchd refuses a plist anyone but root can write.
sudo install -m 644 -o root -g wheel \
  packaging/macos/com.mwe-mcp.server.plist /Library/LaunchDaemons/
sudo launchctl bootstrap system /Library/LaunchDaemons/com.mwe-mcp.server.plist
```

```bash
sudo launchctl print system/com.mwe-mcp.server         # is it running?
sudo launchctl kickstart -k system/com.mwe-mcp.server  # restart
sudo tail -f /usr/local/var/mwe-mcp/workdir/logs/mwe-mcp.log*  # the server's log
sudo tail -f /usr/local/var/mwe-mcp/logs/launchd.log   # launchd starting it
```

The server writes its own rotating log under the workdir, dated daily; the
plist's `StandardOutPath` catches what launchd sees around it.

To remove it: `sudo launchctl bootout system/com.mwe-mcp.server`.

Moving an existing workdir into place? Copy it before the `chown`, and let
the first boot re-download the bge-m3 weights (~2.2 GB) into the daemon's own
cache — the plist pins `XDG_CACHE_HOME` inside its tree so they never land in
a home directory it does not own.

### Windows — a scheduled task at boot

The task definition is [`packaging/windows/mwe-mcp-task.xml`](packaging/windows/mwe-mcp-task.xml).
**A scheduled task, not a Windows service:** `mwe-mcp.exe` is a console
program and does not speak the service control protocol, so a service made
with `sc.exe create` is killed at start with error 1053. The task gives the
two properties that actually matter — it starts at boot, and it runs as an
account you never log in with.

Run these in an **elevated** PowerShell, from the folder you unzipped:

```powershell
# 1. The dedicated local account, kept out of the interactive path.
$cred = Get-Credential -UserName mwe-mcp -Message "choose a password for the service account"
New-LocalUser -Name mwe-mcp -Password $cred.Password -PasswordNeverExpires `
              -Description "mwe-mcp service account" -UserMayNotChangePassword
# Then deny it interactive and network logon in secpol.msc → Local Policies →
# User Rights Assignment ("Deny log on locally", "Deny access to this computer
# from the network"). It only ever needs to run as a batch job.

# 2. The binary and its tree, readable by that account and nobody else.
New-Item -ItemType Directory -Force "C:\Program Files\mwe-mcp",
                                    "C:\ProgramData\mwe-mcp\workdir",
                                    "C:\ProgramData\mwe-mcp\cache"
Copy-Item .\mwe-mcp.exe "C:\Program Files\mwe-mcp\"
icacls "C:\ProgramData\mwe-mcp" /inheritance:r `
       /grant "mwe-mcp:(OI)(CI)F" /grant "Administrators:(OI)(CI)F" /T

# 3. Where the bge-m3 weights (~2.2 GB) go. Machine-wide, because on Windows
#    the server has no home directory to fall back on and would otherwise
#    write them relative to its working directory.
[Environment]::SetEnvironmentVariable(
  "XDG_CACHE_HOME", "C:\ProgramData\mwe-mcp\cache", "Machine")

# 4. The task itself, running as that account.
Register-ScheduledTask -TaskName mwe-mcp `
  -Xml (Get-Content .\packaging\windows\mwe-mcp-task.xml -Raw) `
  -User $cred.UserName -Password $cred.GetNetworkCredential().Password
Start-ScheduledTask -TaskName mwe-mcp
```

```powershell
Get-ScheduledTaskInfo -TaskName mwe-mcp    # last run, last result
Stop-ScheduledTask  -TaskName mwe-mcp
Start-ScheduledTask -TaskName mwe-mcp      # restart, after a binary swap
Get-Content "C:\ProgramData\mwe-mcp\workdir\logs\mwe-mcp.log*" -Wait
```

To remove it: `Unregister-ScheduledTask -TaskName mwe-mcp -Confirm:$false`.

The task file ships in the release archive alongside the binary, so
`.\packaging\windows\mwe-mcp-task.xml` is where you unzipped it.

---

## Where your data lives

Everything mwe-mcp owns lives under `--workdir`:

- `wikis/` — the memory as plain Markdown prose (portable files, yours to back up and version; the reading surface is the dashboard's memory explorer),
- `engine.db` — the per-fact governance index (ACL, validity, attribution, vectors),
- the config and `mwe-mcp.env` (secrets).

Snapshot that one folder and you've backed up the whole memory.

**What the server keeps, and for how long.** Three things grow with use and
are swept once a day, on the windows in the `retention:` section of
`mwe-mcp.config.yaml` — `0` on any of them means keep for ever:

| Key | Default | What it bounds |
|---|---|---|
| `retention.audit_days` | 90 days | The per-call audit trail: who called what, when, and did it fail. |
| `retention.undo_days` | 30 days | The page bodies a smart consumer's push overwrote — **this is the undo window**: past it the operation log still says who pushed what, and the Revert button is gone. |
| `retention.trash_days` | 30 days | Deleted wiki subtrees, which are moved to `<workdir>/trash/` rather than erased. Past the window they are erased. Anything you put in `trash/` yourself is never touched — the sweep only removes directories whose name carries the deletion stamp the server wrote. |

Two more windows live in the sections that own what they bound:
`usage.retention_days` (the per-call token ledger, 400 days) and
`recall.trace_retention_days` (the recall-trace journal, 90 days, which holds
recalled memory verbatim).

---

## What it costs, and a daily budget that stops it

**Admin → Usage** is the page for what this deployment consumes: today's spend
against your budget, then the history — by day, by month, by slot, by model,
tokens in and out and how much of the prompt the provider's cache absorbed. The
embedder gets a row of its own when it runs over the wire.

**Tokens are the measurement; money is your price list.** No rates ship with the
product: published prices change, your contract may not be the published one,
and the currency is not ours to assume, so a figure invented on your behalf
would be confidently wrong about your money. Fill the price list in on that
page — per 1M tokens, in whatever currency you are billed in — and the cost
columns appear. A model id may be a `prefix*` wildcard, and the longest match
wins whatever order you wrote the rows in.

```yaml
llm_pricing:
  currency: EUR
  models:
    - model: "gemini-3-flash-*"
      input: 0.30
      cached_input: 0.075   # cache read; omitted ⇒ same as input
      cache_write: 0.375    # cache write; omitted ⇒ same as input
      output: 2.50

budget:
  daily_limit: 5.00       # in llm_pricing.currency; omit the key for no budget
  warn_at_percent: 80     # warn once a day at this share of the budget
```

**What the budget does.** Reach `warn_at_percent` of it and you are told once
that day, on the dashboard and on the reverse channel (`events_poll`, kind
`budget_threshold_reached`). Reach the budget itself and **paid model calls
stop** until 00:00 UTC, or until you raise the budget or press *Unlock for
today* on that page.

Three things are worth knowing before you set one:

- **It covers metered calls only.** A slot on a flat subscription, or a model
  running on your own machine, moves tokens without moving money, so it never
  counts towards the budget and is never stopped.
- **It is read against your price list.** A model you have not priced spends
  nothing as far as the budget is concerned — the page says how many of today's
  calls that is.
- **A stop is not a way to run without a model.** All six model roles stay
  required. A stopped deployment answers user turns degraded — the turn says
  nothing was saved rather than dying — and the nightly cycle skips its round
  and says why. It is a pause you chose, and it lasts until you take it back.

> **Keep the workdir private.** The Markdown under it is **cleartext on disk** —
> per-reader redaction happens when the server renders a response, not on disk.
> Keep the workdir on a machine/user that is allowed to see the memory, and
> `chmod 700` it. `mwe-mcp serve` warns on a world-/group-readable workdir and
> `mwe-mcp doctor` reports every loose path with a fix. Both read POSIX mode
> bits, so on Windows, where permissions are ACLs, the boot warning stays
> silent and `doctor` says the permissions were not inspected — the workdir is
> yours to lock down there. For a **multi-user** memory
> or when the consumer agent runs with shell/file tools, read the topology rules in
> [`INTEGRATING.md`](INTEGRATING.md#deployment-security--where-to-run-the-consumer).

---

## Hardening checklist

The defaults are already conservative; production exposure adds seven habits:

1. **Keep the bind on loopback** (both the exposure prompt and the
   non-interactive default resolve to `127.0.0.1:8742`) and expose the port
   through a TLS reverse proxy or an
   authenticated tunnel (Cloudflare Tunnel, Tailscale, a VPN). Never forward
   bare HTTP across a network you don't own — every request carries a bearer
   token. If that fronting layer runs bot or browser-integrity filters, exempt
   `/mcp` from them: it is an API path spoken only by programs, and those
   filters reject non-browser clients on sight — a consumer then gets an
   opaque `403` from the edge that looks like a revoked token but never
   reaches mwe-mcp at all. `/dashboard` is the browser surface; leave its
   filtering alone.
2. **Tell the server the address people reach it at.** Set
   `public_base_url` in `mwe-mcp.config.yaml` (or from Settings → *Public
   address of this server*) to the address your tunnel or proxy publishes,
   e.g. `https://memory.example`. Three things are built on it: the
   password-reset link, the invitation email, and the link an agent mints
   with `dashboard_link` so somebody can open their own memory. The server
   never derives it from the request — an address taken from the browser's
   `Host` header is one whoever sent the request chose, and these links
   carry credentials — so **without it the two emails are not sent at
   all** (the dashboard still shows you the link to hand over) and
   `dashboard_link` answers with a path for the consumer to complete.
   `https://` is required away from the machine itself; `http://` is
   accepted only for a loopback host, which is the documented first run.
3. **Once the dashboard is behind TLS, mark its cookies for HTTPS only.**
   Set `instance.cookie_secure: true` in `mwe-mcp.config.yaml` and
   restart: the session, reveal and 2FA cookies are then sent by the
   browser over HTTPS alone. It is off by default only because the first
   run is plain `http://127.0.0.1:8742`, where such a cookie would never
   come back.
4. **Treat tokens as per-consumer credentials.** Mint one token per agent
   from the dashboard, scope it with its delegation list at mint time, and
   revoke it there the moment the consumer is retired. The signing secret
   lives in the workdir's `mwe-mcp.env` — it travels with backups, so backups
   inherit the workdir's confidentiality requirements.
5. **Back up the workdir as one unit.** `engine.db` is the authoritative
   fact store — it is *not* rebuildable from the Markdown — so a backup is
   only valid when it snapshots **both halves together**. The dashboard's
   Backup console takes a hot snapshot of the whole workdir on demand; to
   restore, stop the server and put the snapshot back in place.
6. **Mind who shares the machine.** Per-reader redaction happens at render
   time; the files are cleartext on disk. The workdir permission rules and
   the consumer co-location topology are in
   [`INTEGRATING.md`](INTEGRATING.md#deployment-security--where-to-run-the-consumer)
   — `mwe-mcp doctor` audits the current install and prints fixes on Linux and
   macOS. On Windows it has no mode bits to read, so lock the workdir with
   `icacls` as [Run it as a service](#run-it-as-a-service) does.
7. **Give a busy consumer its own ceiling.** Every token is already held to
   one: 120 calls a minute and 3 000 an hour, of which 30 a minute and 600 an
   hour may be the calls that run a model or an embedding (`wiki_ingest_message`,
   `wiki_ingest_external`, `wiki_navigate`, `wiki_search`, `recall_core_global`).
   Past the ceiling a call comes back `429 rate_limited` with the seconds to
   wait. The numbers are per **token**, so one runaway consumer never spends
   another's allowance, and they are the ones that bound what a stolen token
   can put on your invoice. To give one consumer different numbers, mint its
   token with `--rate-limit-id <name>` and declare the name in
   `mwe-mcp.config.yaml` (the file `mwe-mcp init` writes carries the shape,
   commented out):

   ```yaml
   rate_limits:
     nightly-import:
       model_calls_per_minute: 120
       model_calls_per_hour: 2000
   ```

   A profile states only the numbers it changes; the rest stay at the
   built-in ones. A `rate_limit_id` with no profile of its own falls back to
   `default`, so a token cannot name its way out of a ceiling. Read at boot —
   a change wants a restart.

Updates are a binary swap: stop the server, replace the binary (keep the old
one as a `.bak`), start — pending migrations run at boot, forward only. Keep
the pre-upgrade snapshot until the new build has served a day: a migration
can rename a column, and the release notes say when one does.

---

## Next: connect an agent

A running server is a memory waiting for a consumer, and your server **serves the
setup itself**: visit `/bridges` for the copy-paste setup per consumer (the
install address is tailored to how you reached the server), or use the
**Bridges** tab once signed in. Three consumers are covered point-and-click
today:

- **[NanoClaw](https://github.com/nanocoai/nanoclaw)** (`/bridges/nanoclaw`) —
  **the ready-made assistant**, and where to start if you have no agent of your
  own. One command places the `mwe` agent template and the `add-mwe-memory` fork
  skill, cloning NanoClaw at the tested ref if you do not have it:

  ```bash
  curl -fsSL http://127.0.0.1:8742/bridges/nanoclaw/install.sh | sh
  ```

  It arrives as a *standard* consumer with this memory as its **only** memory.
  NanoClaw's own prerequisites are Node 22+, pnpm 10+ and Docker (its
  `nanoclaw.sh` installs them), plus Claude Code to apply the skill
  conversationally; on Windows it runs inside WSL2, so run the command there.
  Its step-by-step setup is in
  [`agents-bridges/nanoclaw/README.md`](agents-bridges/nanoclaw/README.md).
- **Claude Code** (`/bridges/claude-code`) — one command plus an OAuth sign-in,
  with **no token to mint or paste**:

  ```bash
  claude mcp add --transport http mwe-mcp http://127.0.0.1:8742/mcp --scope user
  ```

  It connects as a *smart* consumer and authors its own project wikis.
- **[Hermes](https://github.com/NousResearch/hermes-agent)** (Nous Research,
  `/bridges/hermes`) — the **per-turn** plugin bridge for a standard consumer
  you already run, installed with one command. Its step-by-step setup is in
  [`agents-bridges/hermes/README.md`](agents-bridges/hermes/README.md).

The public front page at `/` points an agent straight at the catalog, and each
entry links to a machine-readable `install.md` you can hand to a capable agent.
To wire a host we don't ship a bridge for, the per-turn contract is in
**[`INTEGRATING.md`](INTEGRATING.md)**.

## More

- Full CLI roster: `mwe-mcp --help`, and `--help` on any subcommand.
- The complete config schema: `mwe-mcp init` seeds a commented
  `mwe-mcp.config.yaml`, and the dashboard's settings panels list every knob
  with its default. `mwe-mcp doctor` audits an installation (paths,
  permissions, the env file); run it with the server stopped, it takes the
  workdir lock.
