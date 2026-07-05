# Dogfood — standard consumer (runbook)

Read this to re-form on how to exercise mwe-mcp's **standard-consumer**
surface end-to-end against a real local LLM, the way it was first done
on 2026-05-29. This folder is the reusable apparatus; the dated
*findings* live in the gitignored historical wing at
`road-behind/road-behind/dogfood-standard-2026-05-29.md` (local-only).

> **Update 2026-05-31 (Phase G + diagonal identity).** This runbook now
> reflects three changes since the first run: (1) **`serve` no longer takes
> `--transport`** — HTTP is the only transport (A.8): just `mwe-mcp serve`.
> (2) A **standard consumer is a credential-less system-user "bot"** that speaks
> for humans via `X-MWE-Act-As` (the diagonal identity model); a per-human
> standard token is now **refused** at issuance. (3) **Phase G / the dreams are
> in scope** here, driven over HTTP from the admin **Dream** console
> (`POST /dashboard/dream/{light,compile,full}`) on a strong online model.
> `setup.sh` builds the Baggins cast on the diagonal model; `mcp_client.py`
> sends `X-MWE-Act-As` from `$MWE_ACT_AS`. The 2026-05-31 findings (incl. the
> `allow=` marker fix) are in the historical archive (`road-behind/`).

> **Update 2026-06-01 (ingest v2.6 + base corpus + both bootstrap ways).** The
> `ingest` prompt is **v2.6**: a message is split into an `extractions[]` array
> of atomic facts (the multi-fact-collapse fix). The base prompt corpus that
> seeds the initial wiki — and checks the split inline — is
> [`base-prompts.md`](base-prompts.md); future corpora build on the state it
> leaves. Fixture bootstrap is documented **both ways** below: by hand through
> the dashboard UI (headed, optionally via the Playwright MCP — good for a
> UI/UX check) **and** `setup.sh` (curl, fast).

## The cardinal rule

**Test the server without becoming a consumer.** Drive the MCP tool
surface over raw HTTP with a `consumer_class=standard` JWT. Do **not**
adopt the consumer contract in this session: no hook bundle, no skills,
no `AGENT_INSTRUCTIONS.md` behaviours. You are Claude Code working *on*
the repo (CLAUDE.md), pretending to be a thin standard consumer only at
the wire level. This keeps the dev-session role and the consumer role
from collapsing into each other.

**Phase G / the dreams ARE in scope** (changed 2026-05-31). The narrative
compiler + the REM reorg run on a strong online model (Gemini in the canonical
run); you drive them over HTTP from the admin **Dream** console
(`POST /dashboard/dream/{light,compile,full}`) — no CLI, no workdir-lock
contention. `light` = promote captures + compile dirty pages; `compile` =
recompile dirty only; `full` = reorg + apply parked comments (G.8) + compile.
The embedder stays local (`bge-m3` on Ollama); the workhorse 9B is no longer
the ceiling for ingest/compile quality.

> ⚠️ **A `full` dream does NOT drain the capture buffer** — promotion is the
> `light` dream's job, by design. A replay day driven with *only* `--dream
> full` leaves that day's captures unpromoted (invisible to `wiki_recall` and
> the eval); the 2026-06-11 re-run lost 11 captures of its final day this way.
> Always run a `light` after the last corpus day (and prefer light+full on
> full-dream days).

## What's in this folder

| File | Role |
|---|---|
| `instruction.md` | this runbook |
| `mcp_client.py` | minimal MCP HTTP driver — one `tools/call` per invocation, reads the Bearer JWT from `$MWE_JWT_FILE` |
| `setup.sh` | bootstraps the Baggins fixture on the **diagonal model** (wizard admin → humans → system-user bot → famiglia+amici groups → ONE standard token bound to the bot, humans as act-as senders) — the scripted/curl path; the dashboard-UI path is in §"Setup the fixture" |
| `base-prompts.md` | the **base prompt corpus** that seeds the initial wiki (person + group facts, a shopping list that accumulates across senders, edge cases); atomization (the split) is verified inline; future test corpora build on the state it leaves |
| `.gitignore` | keeps `tokens/` + cookies out of git (they're throwaway JWTs) |

## Two ways to run this

Pick a mode up front — same server, the difference is whether you **watch** it:

- **Visual (non-headless).** Do everything you can through the **dashboard in a real
  browser**: bootstrap (wizard → users → groups → token), trigger dreams from
  the **Dream** console, and read the compiled prose on the page. Drive it by
  hand or with the **Playwright MCP** (`--browser chrome`, non-headless; deps
  already installed) so the UI/UX is verified and you see it live. The only
  thing with no dashboard surface is *sending a user message* (a consumer
  action), so the ingest lines still go over the MCP wire (`mcp_client.py`) —
  everything around them is visual.
- **Endpoints (the dedicated HTTP endpoints).** No browser: bootstrap with **`setup.sh`** (curl on the
  dashboard form endpoints), drive ingest with `mcp_client.py`, trigger dreams
  with `curl -X POST …/dashboard/dream/{light,full}`, inspect with `sqlite3`.
  Fast and scriptable.

Note: **`setup.sh` is the headless bootstrap — it does not launch Playwright.**
The visual bootstrap is the by-hand / Playwright path in §"Setup the fixture"
option (a). Both land on the identical fixture.

## Prerequisites

1. **Embedder up (local).** The embedding model `bge-m3` runs on **Ollama** and
   is the only piece that must be local for the canonical run — the LLM slots
   (`ingest`, `rem_*`, `cronista`, `hub_writer`) point at **Gemini** (see
   `./work/mwe-mcp.config.yaml`). An all-local run with the `qwen3.5:9b-q8_0`
   workhorse on those slots still works, but it is the old pre-v2.6 baseline
   (slower, weaker on the structural / scope / split judgments); the canonical
   dogfood is Gemini.
2. **Server up** on a throwaway, gitignored workdir (`--workdir` defaults to
   `./work`; HTTP is the only transport — there is no `--transport` flag):
   ```bash
   RUST_LOG=info ./target/debug/mwe-mcp serve &
   ```
   Dashboard at `http://127.0.0.1:8742/dashboard`, MCP at `…/mcp`. Boot
   health-checks every configured LLM slot, so the Gemini slots need a
   reachable `GEMINI_API_KEY` in `./work/mwe-mcp.env`.
   **Rebuild + restart after any Rust change** (route handlers are
   compiled in; dashboard JS/CSS in debug builds are read from disk).
3. **Admin + fixture** — created by your chosen mode (see §"Two ways to run
   this" + §"Setup the fixture"): the **dashboard wizard** in a browser (visual
   mode) or **`setup.sh`** (API mode, which runs the wizard for you and falls
   back to login on an existing workdir). The visual mode also needs Chrome —
   by hand or via the Playwright MCP (`--browser chrome`, non-headless). Admin is
   `frodo` / `frodo@shire.test` / the throwaway password `!Brandivino84`
   (`setup.sh`'s default; type the same in the wizard for the visual mode) — an
   already-compromised local-test credential, never a real one; the workdir is
   disposable.

## The mental model (so you don't re-derive it)

These are the load-bearing facts about how the system actually behaves;
they shape what to test and how to read the results.

- **ACL is region-level**, via inline markers
  `{{owner=… allow=… sender=… f=<uuid>}}body{{/}}`. The only principals
  are `user:<id>`, `group:<id>`, `global`. There is no wiki-level
  "scope". Filtering happens region by region (`can_read`).
- **Enforcement is solid and deterministic.** Recall drops invisible
  facts entirely; `wiki_read` renders them inline as the literal
  `[redacted]` (with `redacted_count`). `isAdmin` does **not** bypass
  ACL. This is the part that just works — assert on it confidently.
- **Scope is decided by the ingest LLM, not the caller.** A standard
  consumer only sends `wiki_ingest_message`; the server's internal
  classifier picks `owner_id` (`user:`/`group:`/`global`) and the
  target wiki. **Default when unsure = private** (`user:<sender>`).
- **The classifier is the load-bearing piece — on the canonical run it is a
  strong model (Gemini), not the local 9B.** The `ingest` slot points at Gemini,
  which closes the judgment gaps the old 9B under-triggered (group-scope /
  cross-user **F-A**, public→`global` **F-F**, structural intent **F-H**), and
  prompt **v2.6** fixed the multi-fact **split** (a message → an `extractions[]`
  array; see [`base-prompts.md`](base-prompts.md)). The 2026-05-29 figures (9B
  under-sharing to private, dropping public facts, never reaching `structural`)
  are the **historical all-local baseline** — the strong-model run is expected
  to pass them, and that is what the base corpus measures.
- **Standard capture never forges wikis.** New wikis are meant to
  emerge from an explicit user request → `structural` intent → a nudge
  to the dashboard. `target_page` is normalised server-side now
  (append `.md`, fall back to `index.md` if unsafe — F-B fix).
- **`wiki_read` reads `index.md` only** — regions captured into other
  pages of a wiki are findable via `wiki_search` but invisible to
  `wiki_read`.
- **Transport**: Streamable HTTP, **stateless** (no `Mcp-Session-Id`),
  Bearer JWT. **Group membership is resolved server-side** from
  `enrollment_groups` on every call — it is **not** in the JWT.

The exhaustive code map (file:line) is in the report's "mental model"
sources; re-run a focused `Explore`/grep only if a finding needs a
fresh citation.

## Setup the fixture

Two equivalent ways to build the Baggins fixture — **both land on the same
state** (admin frodo + humans galadriel/gollum/bilbo + the `samvise` system-user
bot + `famiglia`/`amici` groups + one standard consumer token). Pick by what
you're exercising:

**(a) By hand through the dashboard UI — when you also want a UI/UX check.**
With the server up, drive a real browser through
`http://127.0.0.1:8742/dashboard`: the first-run **wizard** (creates admin
frodo), then **+ Add user** for galadriel/gollum/bilbo and the `samvise` bot
(give the bot *no* password → it becomes a system user, which is what lets a
standard token bind to it), then the `famiglia`/`amici` groups, then **Issue
token** (`consumer_token` on, act-as all four humans) and copy the JWT into
`tokens/samvise.jwt`. This is how the fixture was first built. Drive it
**headed** — by hand or via the Playwright MCP (`@playwright/mcp --browser
chrome`, non-headless; deps already installed) — so you watch forms,
validation, and rendering actually work.

**(b) Via `setup.sh` — fast, scripted, no browser.** The same steps issued as
HTTP form POSTs with `curl`; convenient when the UI is not what you're testing:

```bash
cd tests/dogfood-standard
bash setup.sh   # wizard frodo + humans + bot 'samvise' + groups famiglia/amici + tokens/samvise.jwt
```

`setup.sh` is parameterised (`ADMIN_ID`, `ADMIN_EMAIL`, `ADMIN_PASS`, `BOT_ID`,
`CONSUMER_ID`, `MWE_DASH_URL`). It bootstraps the **Baggins** cast: admin **frodo**, humans
**galadriel/gollum/bilbo**, the consumer bot **samvise** (a system user), and
the groups **famiglia** {frodo,galadriel,gollum} + **amici** {frodo,bilbo}. The
famiglia scope is the gradient (groceries / house rules / shared plans /
presence / kids' school are *shared*; personal passwords + irrelevant-personal
facts stay *private*) — the whole point of the central test below. amici is a
second group, so a fact shared with bilbo is invisible to the family and vice
versa.

Result on disk after a dream: `./work/wikis/{frodo,galadriel,gollum,bilbo}/`
(person wikis), `…/{famiglia,amici}/` (group wikis), plus `…/samvise/` (the
bot's own, unused for captures). Verify:

```bash
sqlite3 ./work/engine.db "SELECT user_id,is_admin FROM enrollment_users;"
sqlite3 ./work/engine.db "SELECT group_id,members,substr(scope,1,50) FROM enrollment_groups;"
sqlite3 ./work/engine.db "SELECT consumer_id,allowed_sender_ids FROM consumer_delegations;"
```

## First-access fields — what to type, per user

The endpoints path (`setup.sh`) fills all of this for you; this section is for the
**visual** path — exactly what to type in each wizard/form field, grounded in the
fictional Baggins cast below.

**Password — one for everyone.** Use **`!Brandivino84`** for every account that gets a
credential (the admin, plus any human you onboard to exercise their welcome wizard).
It is internet-compromised, but the local test network is closed and self-managed —
no remote attacker — so reusing one throwaway password across all test users is fine
and saves remembering several. It is `setup.sh`'s `ADMIN_PASS` default.

**1 — first-run setup wizard** (`/dashboard/setup`):

| field | value |
|---|---|
| Email | `frodo@shire.test` |
| Admin id (slug) | `frodo` |
| Password + Confirm | `!Brandivino84` |

Creates admin **frodo** and lands on the welcome primer.

**2 — welcome primer** (`/dashboard/welcome`, shown on each user's first login). 14
optional fields → composed into a first-person Italian message → ingested as
**public** profile facts (the wizard prepends a public-consent line, so they land
`owner_id: global`). It doubles as an atomization probe: a multi-field profile is a
multi-clause message that must split into N facts. Fill only what the cast sheet
supports, leave the rest blank:

| user | display_name | nickname | birthday | address | occupation | hobbies | food_preferences | presentati |
|---|---|---|---|---|---|---|---|---|
| **frodo** | Folco | Padron Folco | 1984-05-23 | Ferrara | lavoro alla Martinelli | — | — | "Comunicazione diretta, niente giri di parole; preferisco le soluzioni pratiche." |
| **galadriel** | Galadriel | Nina | 1993-08-14 | — | — | leggere | — | "Tono riverente e poetico, ma concisa." |
| **gollum** | Matteo | Sméagol | 2018-03-12 | — | — | karate | "adoro il McDonald's, odio il pesto" | — |
| **bilbo** | Bilbo | nonno Bruno | — | — | — | — | — | "Il nonno di casa; conversazione semplice e pratica." |

frodo's primer should split into several public facts (name / nickname / birthday /
city / occupation / the free-text line). gollum has no real login channel — onboard
him only if you want to exercise the wizard as a child account.

**3 — create the other users** (`/dashboard/users/new`):

| user_id | aliases | profile |
|---|---|---|
| galadriel | Morgana, Nina | Compagna di Frodo, madre di Gollum; ama i libri. |
| gollum | Matteo, Sméagol | Figlio di Frodo e Galadriel (nato 2018-03-12); fa karate, ama il McDonald's, odia il pesto. |
| bilbo | Bruno, nonno Bruno | Padre di Frodo, nonno di Gollum; amico di famiglia (gruppo amici). |
| samvise (bot) | *(none)* | Consumer bot (Samvise 2.0), standard-conversational. Give it **no password** → system user. |

frodo's own aliases (Folco, Padron Folco) are not set by the setup wizard; add
them from the user-edit view if you need cross-user attribution to resolve "Folco".

**4 — groups** (`/dashboard/groups/new`): **famiglia** = {frodo, galadriel, gollum},
**amici** = {frodo, bilbo}. Copy the exact `scope` strings from `setup.sh`
(`FAM_SCOPE` and the amici line) — the family scope (shared lists / plans / presence /
kids' school; excludes personal passwords + irrelevant-personal facts) is what the
classifier routes `owner_id` on.

**5 — issue the standard consumer token** (`/dashboard/tokens/issue`): sender
`samvise`, `consumer_token` on, `consumer_id` = `samvise-prod`, act-as
frodo / galadriel / gollum / bilbo. Copy the rendered JWT into `tokens/samvise.jwt`.

## Drive the surface

The base corpus to seed a realistic wiki — and to check atomization (the split)
inline — is [`base-prompts.md`](base-prompts.md); run it first, later corpora
(wiki emergence, REM) assume the state it leaves.

Every call uses the **one bot token + act-as the human**:
`MWE_JWT_FILE=tokens/samvise.jwt MWE_ACT_AS=<human> python3 mcp_client.py <tool> '<json>'`
(`$MWE_ACT_AS` sets `X-MWE-Act-As`; the server resolves it against the bot's
delegation, so the fact is attributed to that human). Tool arg names:
`wiki_ingest_message{text,context_hint,recent_messages,metadata}`,
`wiki_search{query,scope?}`, `wiki_read{wiki_id}`,
`structure_proposal_list{}`, `events_poll{consumer_id}`. Captures land in the
buffer (`_captures.md`); run a dream to promote + compile:
`curl -b tokens/cookies.txt -X POST http://127.0.0.1:8742/dashboard/dream/light`.

### 1. ACL enforcement baseline (deterministic — should always pass)

Capture a private fact and a global one as user A, then read as user B:

```bash
A=tokens/frodo.jwt ; B=tokens/miriam.jwt
MWE_JWT_FILE=$A python3 mcp_client.py wiki_ingest_message '{"text":"Il mio cane si chiama Pippo.","context_hint":"conversation"}'
# A sees it; B must NOT (recall excludes it; wiki_read shows [redacted])
MWE_JWT_FILE=$A python3 mcp_client.py wiki_search '{"query":"cane Pippo"}'   # total includes it
MWE_JWT_FILE=$B python3 mcp_client.py wiki_search '{"query":"cane Pippo"}'   # total excludes it
MWE_JWT_FILE=$B python3 mcp_client.py wiki_read  '{"wiki_id":"frodo"}'       # private region -> "[redacted]", redacted_count>=1
```

### 2. Scope-gradient — THE central probe

Capture each line **as user A**, then read where it landed. The
*correct* outcome follows the famiglia gradient; the 2026-05-29 baseline
shows several under-sharing (F-A) — that's the regression target.

| Utterance (as A) | Correct scope |
|---|---|
| "oggi ho corretto un bug di lumen" | `user:A` (personal) |
| "la password del NAS è …" | `user:A` (private) |
| "metti il detersivo nella lista della spesa di casa" | `group:<group>` |
| "sabato cena di famiglia con i Brandibuck a casa nostra" | `group:<group>` |
| "domani lavoro solo il pomeriggio" (presence) | `group:<group>` |
| "i compiti dei bambini sono per lunedì" (school) | `group:<group>` |
| "informazione pubblica, visibile a chiunque: il mio sito è X" | `global` |
| (as B) "ho finito un romanzo" | `user:B` (private) |

Inspect what the classifier decided — the response does **not** expose
`owner_id`, so read it from the index:

```bash
sqlite3 -header ./work/engine.db \
  "SELECT wiki_id, owner_id, fact_type, substr(replace(text,char(10),' '),1,50) \
   FROM fact_index WHERE deleted_at IS NULL AND superseded_at IS NULL \
   ORDER BY created_at DESC LIMIT 10;"
```

Then confirm the *consequence* as B: a `group:` fact is recallable by
B; a `user:A`-private one is not. Inspect the raw marker on disk to see
`owner=`/`sender=`:

```bash
sqlite3 ./work/engine.db "SELECT source_path FROM fact_index WHERE text LIKE '%detersivo%';"
cat "./work/wikis/<group>/<that_source_path_basename>"
```

### 3. The rest of the checklist

- **Capture types**: vary fact types (bio/preference/state/rule/plan/episode); confirm regions land with `{{f=…}}` markers.
- **Recall intent**: "Cosa sai del mio cane?" → `intent_classified:"recall"`, `context_snippet` filled, no capture.
- **Supersede**: a correction whose subject is recalled this turn
  ("in realtà il cane si chiama Argo") → old fact `superseded_at` set,
  `superseded_by` chained. Make the correction semantically close to
  the original so recall surfaces it.
- **Dedup**: a near-identical re-statement is usually handled as a
  *supersede* by the LLM (not the lexical Jaccard skip, and not the REM
  revisor). Note which mechanism fired.
- **Disambiguation**: `needs_disambig` is LLM-driven and hard to force
  with natural messages; if it doesn't trigger, say so.
- **Structural / wiki emergence**: "voglio un quaderno per le ricette"
  → *should* be `intent:"structural"` (a dashboard nudge). Baseline:
  it isn't (F-H).
- **Direct tools**: `wiki_search` / `wiki_read` (ACL-filtered, index-only);
  `events_poll{consumer_id}` returns `consumer_not_registered` for a
  mono-user standard token (needs `consumer_register` first).
- **Proposals**: `structure_proposal_list` works (its `total` is the
  page size, not a real count).

## Interpreting results

- A wrong **enforcement** result (B sees A's private fact) is a real,
  high-severity bug — escalate immediately.
- A wrong **classification** result (scope/intent) is expected today
  (F-A/F-H/F-F) and is a *measurement*, not a regression — record the
  decision, compare to the report's baseline. After the F-A/F-H fixes
  land, these become pass/fail.
- Distinguish "the LLM chose X" (probabilistic, re-run to gauge) from
  "the code did X" (deterministic).

## Reset / teardown

The workdir is disposable and gitignored. For a clean run:

```bash
# Stop by EXACT process name, not `-f '…serve'`: a `-f` pattern that contains the
# string "mwe-mcp serve" also matches the shell running pkill, SIGINT-ing itself.
pkill -INT -x mwe-mcp
# Clean the MEMORY state but KEEP config + env — the Gemini key + token secret
# live in ./work/mwe-mcp.env, so a full `rm -rf ./work` would nuke them.
# Wipe ./work/prompts too: a STALE operator prompt override there WINS over the
# bundled prompt (a leftover v2.4 ingest.md once masked v2.6 and looked like a
# regression). A clean dogfood must run the bundled prompts.
rm -rf ./work/engine.db ./work/engine.db-shm ./work/engine.db-wal \
       ./work/wikis ./work/logs ./work/.mwe-mcp.lock ./work/prompts && mkdir -p ./work/wikis
# restart `mwe-mcp serve`, then re-run setup.sh (it redoes the wizard).
```

Tokens in `tokens/` survive a server *restart* (the secret lives in
`./work/mwe-mcp.env`) but not a memory wipe — re-run `setup.sh` after one. To
force a full recompile (e.g. to re-verify the compiler), also delete the plan
(`rm -f ./work/wikis/_plan/*.json`) before the next `dream/compile`.

> ⚠️ **Deleting the plan = rebuilding the topology, not just re-rendering.**
> With no persisted plan the Cartografo re-derives the page layout from
> scratch (no carry-over), `prepoint_plan_moves` re-points every fact onto the
> fresh pages, and the compile-tail **orphan sweep then deletes the old page
> files** (they hold no live pointers). No fact is lost — verified 2026-06-11:
> 382/382 rows re-pointed, zero dangling paths — but the on-disk file layout
> is replaced wholesale. Don't do it casually on a state you still want to
> inspect.

## Pointers

- Dated findings + verdict: `road-behind/road-behind/dogfood-standard-2026-05-29.md` (gitignored historical wing, local-only)
- Open findings: `wiki/roadmap.md` (the remaining-work list)
- Chronology: `road-behind/road-behind/road-behind.md` § the relevant date (historical log)
- Tool surface spec (for the consumer side): `AGENT_INSTRUCTIONS.md`
- Engineering wiki for the ingest path: `wiki/design-notes/ingest-pipeline.md`
