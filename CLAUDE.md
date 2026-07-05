# mwe-mcp — instructions for Claude Code (and any other LLM working in this repo)

This file is loaded into every Claude Code session in this repository. It is the
**collaboration contract**; the engineering wiki ([`wiki/index.md`](wiki/index.md))
is where you learn what the system is. Onboarding a contributor or agent: point
them at the wiki first, then here.

## 1. What this repo is

**mwe-mcp** is an agent-agnostic **MCP server** that gives any LLM agent a
persistent, structured memory shaped like an Obsidian-native wiki. **The
engineering wiki ([`wiki/`](wiki/index.md)) is the single source of truth — start
there.** It documents what the system is and does **today**; the forward-looking
list of what is still to build lives in the same wiki, in
[`wiki/roadmap.md`](wiki/roadmap.md).

## 2. Terminology — the one disambiguation you must internalise

The word "wiki" means two completely different things here; mixing them up corrupts
every conversation about the system. Use the qualified term whenever context could
be ambiguous.

| Term | Refers to | Where it lives |
|---|---|---|
| **engineering wiki** *(dev wiki)* | Docs for people working on mwe-mcp itself — the SSOT for current state. Contributor-facing, English. | [`wiki/`](wiki/index.md) — this repo |
| **memory wiki** *(consumer wiki)* | The persistent memory mwe-mcp manages at runtime for a consumer agent (Claude, Cursor, OpenClaw, …). The **product** of mwe-mcp, not documentation. | `<workdir>/wikis/<wiki_id>/` — **outside** this repo (operator-chosen `--workdir`) |
| **road-behind** | The frozen Italian planning corpus; **gitignored**, local-only; a reference of last resort. Its canonical content has been absorbed into the engineering wiki. | `road-behind/` at the repo root — out of the tracked repo |

- **Never write bare "wiki"** unqualified in code, comments, the engineering wiki,
  commit messages, or PRs — always "engineering wiki" or "memory wiki".
- **Never put memory-wiki data inside the repo**, even temporarily for testing —
  use `--workdir ./work` (which `.gitignore` excludes).

## 3. Repository layout

The full file map is the "Section index" of [`wiki/index.md`](wiki/index.md).
Orientation only:

- **`crates/`** — `mwe-core` (headless engine), `mwe-mcp-server` (the `mwe-mcp`
  binary), `mwe-dashboard` (built-in PWA).
- **`agents-bridges/`** — in-repo host adapters (one directory per host
  framework, Python/TS, outside the cargo workspace) implementing the per-turn
  contract of `INTEGRATING.md`; authoring guide in
  [`agents-bridges/README.md`](agents-bridges/README.md), machinery documented in
  [`wiki/development/agents-bridges.md`](wiki/development/agents-bridges.md).
- **`migrations/` · `schemas/` · `static/` · `tailwind/` · `tests/` · `examples/`** —
  sqlx migrations (embedded at compile time), JSON Schemas, rust-embedded PWA
  assets, Tailwind sources, integration tests, consumer starters.
- **`wiki/`** — the engineering wiki: the SSOT (English, lockstep with code). It
  also holds [`roadmap.md`](wiki/roadmap.md) (forward work), the per-area detail
  pages under [`wiki/planning/`](wiki/planning/), and [`logs.md`](wiki/logs.md) (the
  decision log).
- **Root docs** — `README.md`, `INSTALL.md`, `INTEGRATING.md`,
  `AGENT_INSTRUCTIONS.md`, `CHANGELOG.md` (each surface's scope is §4.bis).
  Workspace config:
  `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`/`clippy.toml`/`deny.toml`,
  `.cargo/config.toml`, `.github/workflows/ci.yml`.
- **`road-behind/`** — gitignored historical archive (§2). Don't edit it or link to
  it from tracked files.

Two look-alikes that are **not** for Claude Code editing this repo:

- [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) — how a *consumer* agent uses the
  MCP surface (for the agent whose memory mwe-mcp manages, **not** for you).
- [`wiki/wiki-lookup-guide.md`](wiki/wiki-lookup-guide.md) — the engineering wiki's
  internal navigation guide, used together with this file.

## 4. How the docs work — the wiki is the SSOT, in lockstep with code

All tracked documentation lives in `wiki/`, split into two halves:

- **What is built** — the per-area pages (`concepts/`, `architecture/`,
  `design-notes/`, `protocol/`, `development/`, `examples/`). Each describes the
  **current state** only; its `status:` badge marks maturity on the per-area
  ladder `scaffold` → `partial` → `implemented` → `stable` (the non-area
  surfaces below carry their own status vocabulary). **No history, no tombstones**: when an
  area changes, rewrite its page; when it's removed, delete the page. The wiki never
  narrates "we used to do X" or "superseded by Y" — only what is true now.
- **What is left to build** — [`wiki/roadmap.md`](wiki/roadmap.md): a single
  numbered checklist (`1a`/`1b`/`2a`…) of **remaining** work, done/undone markers,
  each item linking to one detail page under [`wiki/planning/`](wiki/planning/).
  Completed phases are not listed — that story is told by the per-area pages above.
  The roadmap page itself carries `status: living`; each planning detail page
  carries `status: planned` / `in-progress` / `gated` (blocked on a prerequisite)
  to mark where its work stands.
- **The decision log** — [`wiki/logs.md`](wiki/logs.md): append-only, newest first,
  one line per design decision with a link to the page it changed (`status: living`).

**When you change code:**

1. Make the change.
2. Update the wiki page for that area **in the same commit**. Bump `status:` as the
   area matures; bump `last_review:` to today **only if you verified the page
   against the code** (not on cosmetic edits). **Never hardcode a derived count**
   (tools, sub-jobs, migrations, templates) — point at the code SSOT
   (`schemas::all_tools()`, `rem::run_cycle`, `migrations/`); keep at most one
   canonical count where a page is pedagogically the roster
   ([`wiki/protocol/mcp-tools.md`](wiki/protocol/mcp-tools.md)).
3. If the change advances a **roadmap** item, tick its sub-step in
   [`wiki/roadmap.md`](wiki/roadmap.md) (and update its detail page); when a whole
   area lands, remove it from the roadmap and make sure the per-area pages document
   it.
4. If the change moves a claim the **README** surfaces, update it in the same
   commit (README spec: §4.bis).

**When you make a design decision:** put the substance on the right wiki surface
(the per-area page if it's how the system works, the roadmap/detail page if it's
forward), then append a one-line dated entry to
[`wiki/logs.md`](wiki/logs.md) — date + area + 1-line + link to the target.

**`last_review` is the date only** (`"2026-06-09"`), no prose — **replace** the
value each time, never append or chain. (`logs.md` is the exception: append-only.)

**One execution rule that pays off: verify against the code before trusting a
doc or a recalled memory.** Both drift. Grep the code SSOT and distinguish
phasing / a documented incremental choice / a real gap.

## 4.bis Where things go

Keep these surfaces disjoint or they drift.

| Surface | Scope | NOT for |
|---|---|---|
| **[`wiki/`](wiki/index.md)** per-area pages (the SSOT) | What the system **is** and does today: data model, runtime behaviour, the protocol surface, configuration, algorithms, invariants, worked examples. Lockstep with code. English, current-state only. | Forward plans. History/tombstones. Per-session narrative. |
| **[`wiki/roadmap.md`](wiki/roadmap.md)** + [`wiki/planning/`](wiki/planning/) | **Only remaining work**: the numbered checklist + one detail page per area, with the open design decisions for each. English. | Current-state behaviour (→ the per-area pages). Completed work. |
| **[`wiki/logs.md`](wiki/logs.md)** | Append-only chronological log of design decisions: date + area + 1-line + link. | Mutating old entries. A snapshot of current state (→ the wiki/roadmap). |
| **`road-behind/`** (gitignored) | The frozen Italian planning corpus. Read-only reference of last resort. | Anything live. **Don't edit it or link to it from tracked files.** |
| **[`README.md`](README.md)** | The **GitHub front page**: product-voice presentation of what mwe-mcp is and why it's worth trying. **Standalone**: no startup/`cargo` commands; the **only** outgoing repo-doc link is `INSTALL.md`. Updated only when a change moves a claim it surfaces. English. | Internal phase nomenclature. Mirroring the wiki. Startup/`cargo` commands or deep integration detail (→ `INSTALL.md`). |
| **[`INSTALL.md`](INSTALL.md)** | The **easiest path to a running standalone server**, operator-voice (not developer-first): download-and-run the prebuilt binary, `serve`, finish setup in the dashboard (admin → LLM provider → profile primer → token), where the workdir lives. Hands off to the dashboard **Bridges** tab (or `INTEGRATING.md`) for connecting an agent. English. | The host-bridge contract / agent wiring (→ `INTEGRATING.md`). Build-from-source depth + full CLI (→ `wiki/development/build-run.md`). Marketing (→ README). |
| **[`INTEGRATING.md`](INTEGRATING.md)** | The **developer & operator guide** for a running server: the per-turn host-bridge contract (v1) for **writing a bridge** for a host we don't ship, where to run the consumer (deployment-security topology), MCP transport / auth. English. | The point-and-click *connect a ready-made consumer* path — that's the dashboard **Bridges** tab / `/bridges` installer (Hermes today), which the server walks the user through. Standing the server up / install / first-config (→ `INSTALL.md`). The consumer-agent runtime contract (→ `AGENT_INSTRUCTIONS.md`). Marketing (→ README). Authoritative behaviour spec (→ the wiki). |

## 5. Build / test / run

```bash
cargo check    --workspace
cargo build    --workspace
cargo test     --workspace --all-targets
cargo clippy   --workspace --all-targets -- -D warnings
cargo fmt      --all
cargo doc      --workspace --no-deps
```

CI replicates these on every push ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) —
don't merge when CI is red. CLI subcommands, flags, and the first-time-setup
walkthrough live in [`wiki/development/build-run.md`](wiki/development/build-run.md).

## 5.bis The `work/` test workdir

`work/` (gitignored, the default `--workdir ./work`) is a **throwaway test
workdir** — disposable dogfood fixtures, **no real data**. Treat it as yours to use
freely: wipe the memory wikis, re-run setup, regenerate, run live ingest / REM /
dream **without asking**. **Always preserve `work/mwe-mcp.env`** (it holds the API
keys + token secret) — so reset the *memory* only:

```bash
rm -rf ./work/engine.db ./work/wikis ./work/media ./work/logs ./work/.mwe-mcp.lock ./work/prompts
```

(`./work/media` is the content-addressed blob store; its catalog lives in
`engine.db`, so wiping one without the other strands orphan rows or blobs.)

**Wipe `./work/prompts/` on purpose:** an operator override at
`<workdir>/prompts/<name>.md` **wins** over the bundled prompt, so a stale override
silently shadows the shipped one. A clean dogfood must exercise the **bundled**
prompts; re-add an override only when you are deliberately testing that path. The
apparatus to repopulate the fixture is in
[`tests/dogfood-standard/`](tests/dogfood-standard/instruction.md).

## 5.ter Local all-local dogfood runtime

The maintainer's dogfood box runs the **all-local** LLM profile (Ollama backend,
set from the dashboard → Admin → LLM config, documented in
[`wiki/design-notes/admin-llm-config.md`](wiki/design-notes/admin-llm-config.md)).
Host/operational facts worth keeping in mind — **not** pinned decisions (the
concrete model picks live in the dashboard config and will drift):

- **Host GPU**: NVIDIA RTX 5060 Ti, 16 GiB, Blackwell **sm_120**. Prebuilt
  PyTorch/CUDA wheels before PyTorch 2.7 / CUDA 12.8 lack the sm_120 kernel and
  crash with `no kernel image is available` even though `cuda.is_available()` is
  `True` — prefer an ONNX embedder, or a `cu128`+ wheel / a source rebuild.
- **All-local models**: workhorse ≈ a local 7-9B-class model via Ollama; embedding
  `bge-m3` (1024-dim). With both models hot plus a ~4 k context the box sits ~93 %
  of 16 GiB — size all-local prompts for ≤ ~3 k input so the reply has room.
- **`think:false` is mandatory for Qwen 3.x workhorse calls.** Qwen 3.x is a
  reasoning model: Ollama otherwise streams reasoning into `message.thinking` and
  can leave `message.content` empty under a small `num_predict`. Already enforced
  in the Ollama client (`crates/mwe-core/src/llm.rs`); any *new* local-workhorse
  call must do the same. (The Ollama backend ignores `reasoning_effort`.)

## 6. Conventions

Full list in [`wiki/development/conventions.md`](wiki/development/conventions.md).
The hard rules:

- **Rust 1.88**, edition 2024, pinned in `rust-toolchain.toml`. MSRV bumps need a
  decision logged in [`wiki/logs.md`](wiki/logs.md).
- `#![forbid(unsafe_code)]` on every crate — no exceptions without a decision + a
  `// SAFETY:` comment at the use site.
- `cargo fmt --all` before commit (CI rejects a diff); `cargo clippy -- -D warnings`
  (allow-list in [`.cargo/config.toml`](.cargo/config.toml)).
- Tracing writes to **stderr** only (stdio MCP reserves stdout).
- Dependencies live in `[workspace.dependencies]`, referenced from crates with
  `name = { workspace = true }`; pin versions. `cargo deny check` enforces the
  license allow-list ([`deny.toml`](deny.toml)) and bans wildcards.

## 7. Languages

**English for the whole product / repo surface** — code, comments, commit
messages, **dashboard / web-page UI strings** (labels, buttons, flash + error
messages, role guidance), **prompts** (`prompts/*.md`), **skills**
(`crates/mwe-core/skills/*.md`), the engineering wiki (incl. roadmap, planning
details, logs), and public docs (README, INSTALL, INTEGRATING,
AGENT_INSTRUCTIONS, CHANGELOG, CLAUDE.md).

- **Chat with the maintainer**: **Italian** (the maintainer's language). They may
  write in either language; reply in Italian.
- **The `road-behind/` archive**: frozen **Italian** — read-only, don't bilingualize
  or edit it.
- **The §11 PILASTRI block below**: the maintainer's own Italian — leave it as-is.

## 7a. Asking the maintainer about ambiguities

When you hit a genuine ambiguity — about a forward item in
[`wiki/roadmap.md`](wiki/roadmap.md) / its detail page, or about how a shipped
behaviour is meant to work — two or more defensible readings where guessing wrong
forces a rework — **stop and ask the maintainer**; don't paper over the gap.

- **Ask in chat as plain prose, not via the structured multiple-choice tool** —
  multiple-choice loses the discursive context. Frame each ambiguity: (1) where it
  lives, (2) the defensible readings, (3) the concrete downstream effect of each.
  You **may group a small set of well-framed confirmations** in one message.
- **Wait for the answer** before writing code that depends on the unresolved branch.
  Independent work may proceed.
- **After the answer, record it** (the per-area page if it's how the system works,
  the roadmap/detail page if forward) and log it in
  [`wiki/logs.md`](wiki/logs.md) so the next session doesn't re-ask.

**Not worth asking about:** code-style, naming, internal API shape, test-framework
picks, file layout inside a crate — make those calls yourself and keep moving.

## 7b. Choose the better way, not the easy one

When you make a code-style, naming, internal-API-shape, test-framework, or
file-layout call: choose the better way, not the easy one.

## 9. What NOT to do (the non-obvious ones — the rest is the inverse of §2/§4/§4.bis)

- ❌ **Create a new git branch on your own initiative.** Branching is the
  maintainer's call: **ask explicitly and wait for an OK** — even on `main`. This
  **overrides** the default "branch first on the default branch". Switching to a
  branch the maintainer named, or staying on the current one, is fine; only
  *creating* one needs permission.
- ❌ **Run any git history mutation (`commit` / `push` / `tag` / version bump) or
  deploy prod on your own initiative** — those are the maintainer's (§10). Do the
  work, leave the working tree coherent, say what's ready, and stop.
- ❌ Skip `--workspace` on cargo commands — per-crate runs miss inter-crate
  compilation issues.
- ❌ Add a build-time dependency that pulls **OpenSSL** — use `rustls-tls`
  consistently (`reqwest`, `hyper-rustls`, `tokio-rustls`).
- ❌ Commit a compiled `static/` asset — embed via `rust-embed` from sources in
  `tailwind/` and `static/`.
- ❌ Leave a wiki page narrating history (supersede notes, "used to", phase-churn).
  The wiki carries the **current state only** — rewrite or delete instead.
- ❌ Reference the gitignored `road-behind/` archive, or internal phase/decision IDs
  (`A.x`–`I.x`, "Phase G", slices, bricks, `§8.bis`), from **code comments, UI
  strings, or the wiki**. Documentation lives in the wiki: point a comment at the
  wiki page that documents that area, not at a planning artifact.
- ❌ Mark a roadmap item done before the wiki per-area pages reflect the
  implementation.

## 10. Git, versioning & prod deploy — the maintainer's, never yours

**Every git history-mutating action, and the prod deploy, belong to the
maintainer.** Several Claude sessions often share the same working tree at once, and
automated git causes messes ("pastrocchi"). So, regardless of any harness default to
commit or branch:

- **Never, on your own initiative, run `git commit` / `git push` / `git tag`, bump
  the version, or update the prod server** — not even when a task "obviously" ends
  in a commit. Make your edits in the working tree, keep it coherent (code + wiki
  **lockstep**, §4, so a later commit captures both together), tell the maintainer
  what is ready, and **stop**. Read-only git (`status`/`diff`/`log`/`show`) is fine
  unasked; mutations are not.
- **The maintainer commits**, because they coordinate the concurrent sessions. Their
  procedure (recorded so you understand the flow — you do **not** run it):
  1. **bump the version** (tag optional);
  2. **one `chore` commit** gathering **all** changes from **all** sessions — if a
     session is still mid-work, the commit waits/aborts rather than capturing a
     half-done tree;
  3. usually-but-optional: **rebuild the binary + update the prod server**.
  (Releases are otherwise tag-driven — pushing `v*` runs `release.yml`; pre-1.0 the
  decision trail is `wiki/logs.md`, not `CHANGELOG.md`.)
- **Prod hotfix exception:** when a fix must reach the live server urgently (e.g. a
  500), you may rebuild + swap the binary **only after the maintainer asks**, always
  keeping a backup of the previous binary — but the working-tree change still stays
  **uncommitted** for the maintainer to fold into their next `chore` commit.

## 11. Ever remember (PILASTRI)

- **ACL per-frammento — l'idea fondante.** Ogni frammento di testo, su ogni riga scritta della **memory wiki**
  (non la engineering wiki), può portare la propria ACL (owner + sender + allow), applicata in scrittura. È la
  granularità «Epstein files style»: il governo dell'accesso scende al singolo frammento. **È quello che le altre
  memorie non hanno — l'idea che ha dato origine a tutto.** Ricordala sempre.
- **Dove vivono i byte (storage DB-autoritativo — ATTERRATO 2026-06-10):** la *capability* per-frammento qui
  sopra è intatta, e l'ACL — con **tutti** i metadati per-fatto — è **autoritativa nel DB** (`fact_index`):
  il marker a runtime è la sola chiave (`{{f=uuid}}`), i `.md` tengono prosa + stile + legami, la redazione
  risolve per chiave, e il marker pieno sopravvive come formato di **export/interscambio** (e input legacy
  valido per sempre). La **granularità per-frammento è il pilastro**; «inline nel testo» era l'implementazione,
  che si è spostata. SSOT: [`wiki/design-notes/redaction-policy.md`](wiki/design-notes/redaction-policy.md) +
  [`wiki/design-notes/marker-grammar.md`](wiki/design-notes/marker-grammar.md) §0.
- **Due `owner` distinti — non confonderli.** **(1) `owner` per-fatto/per-frammento** (nel DB
  `fact_index.owner_id`, sul filo `owner=`, nel tipo `Acl`) è il **soggetto** del fatto — di chi/cosa
  *parla* — **non** chi l'ha scritto (`sender`, provenienza) né chi può leggerlo (`allow`, audience): tre
  assi indipendenti. Il nome resta apposta, non è un refuso Unix/IAM: chi è il soggetto del dato **governa
  chi può leggerlo** (un `acl_change` è owner-or-admin), quindi il soggetto *possiede* davvero il fatto su
  di sé — la stessa idea del primo pilastro vista dal lato del soggetto. Owner può essere un gruppo solo
  quando il soggetto **è** il collettivo (la lista della spesa di famiglia). **(2) `owner` a livello-wiki**
  (`WikiMeta.owner_user`: un utente per la wiki personale, **il gruppo** per una `wiki-group`) è invece il
  **proprietario/master** della wiki — l'owner in senso classico, il controllore degli atti wiki-level, asse
  separato dal soggetto per-frammento (un fatto `owner=user:franz` può vivere in una wiki posseduta da
  `group:famiglia`). Leggi il `owner` per-fatto come *subject*, mai «autore» o «visibilità»; il `owner` di
  wiki come *proprietario*. Una rinomina `owner→subject` è stata valutata e **scartata** vicino alla release
  (il termine copre entrambi i sensi + `owner_id` è anche il token-holder: un replace cieco corrompe, il
  guadagno non vale il rischio ora): il concetto si rinforza, il nome resta. SSOT:
  [`wiki/concepts/identity-and-acl.md`](wiki/concepts/identity-and-acl.md).
