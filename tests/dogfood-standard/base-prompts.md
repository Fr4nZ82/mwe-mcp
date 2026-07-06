# Base prompt corpus — seeds the initial wiki (ingest v2.6)

Companion to [`instruction.md`](instruction.md). This is the **base corpus**:
the set of conversational messages that builds a realistic initial memory for
the Baggins household. **Run this first.** Topic-specific corpora that come
later (e.g. `emergence-prompts.md`, `rem-prompts.md`) assume the state this one
leaves on disk and in the index.

The lines are grounded in the fictional Baggins-household cast (see
`instruction.md`): a coherent set of jobs, kids' activities, and preferences —
so the seeded wiki feels real, not synthetic.

Two things happen as the corpus is ingested:

1. **It seeds the environment** — person wikis, the two group wikis, a shopping
   list several people add to, rules, episodes, a public fact. This is the
   substrate later tests stand on (wiki emergence, REM reorg, recall/ACL).
2. **It verifies atomization inline** — atomization (breaking a message into the
   right number of atomic facts) is *inherent to every capture*, so we check the
   **split** on every line here rather than in a separate test. This is the live
   re-confirmation of the v2.6 prompt fix (multi-fact turns used to collapse into
   one fact; see
   [`ingest-pipeline.md`](../../docs/design-notes/ingest-pipeline.md)
   §"strong model").

> **Maintainer: add your own lines before I run this.** Real messages are
> messier than fixtures — run-ons, several things at once, asides — and that
> mess is exactly what stresses the splitter *and* makes the seeded wiki feel
> human. Add lines in the **"Add your own"** section, written the way a person
> actually texts. I send the whole list only after you've augmented it.

## What we check per line

- **Split (atomization):** a multi-fact message yields **N** distinct atomic
  facts (**never 1**); an atomic message yields **exactly 1**; a not-memorable
  message yields **0** (`skip`); an indivisible fact is **not over-split** (a
  full name is one fact). This is the pass/fail axis for the v2.6 fix.
- **Scope (`owner_id`), as a secondary read:** each fact's principal
  (`user:`/`group:`/`global`), now decided **per extraction**. The exhaustive
  scope-gradient verdict lives in `instruction.md` §2; here it's a sanity note.

## How I send + read each line

Bootstrap the fixture first (both ways in `instruction.md`: §"Setup the fixture"
+ the field-by-field §"First-access fields"). Then drive each line over the MCP
wire (sending a chat message is a *consumer* action, not a dashboard action — and
the dashboard chat panel deliberately does no autocapture):

```bash
cd tests/dogfood-standard
MWE_JWT_FILE=tokens/samvise.jwt MWE_ACT_AS=<as> \
  python3 mcp_client.py wiki_ingest_message '{"text":"<text>","context_hint":"conversation"}'
```

Captures land in the buffer **at ingest time** (no dream needed to see the
split). Count the new rows after each line:

```bash
sqlite3 -header ./work/engine.db \
  "SELECT wiki_id, owner_id, substr(replace(body,char(10),' '),1,70) AS body \
   FROM capture_buffer WHERE status='buffered' ORDER BY rowid DESC LIMIT 12;"
```

(The tool response's `capture_id` is only the **first** fact, so never count
from the response.) Then a `light` dream promotes + compiles, and the compiled
prose can be eyeballed in the dashboard (UI/UX check).

## The corpus

`as` = the `MWE_ACT_AS` human. Cast (fictional): **frodo** (admin; Folco /
Padron Folco; works at Martinelli in Ferrara), **galadriel** (Morgana / Nina;
frodo's partner; loves books), **gollum** (Matteo / Sméagol; their kid, born
2018; karate, loves McDonald's, hates pesto), **bilbo** (Bruno / nonno Bruno;
frodo's father). Groups: **famiglia** = {frodo, galadriel, gollum}; **amici** =
{frodo, bilbo}. Family scope = shared lists/plans/presence/kids'-school; it
**excludes** strictly-personal facts.

### 1 — Identity & bio (builds the person wikis)

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 1 | frodo | "Mi chiamo Folco Baggins, vivo a Ferrara e lavoro alla Martinelli in Via dei Platani." | **3**: name / residence / job (NOT "Folco"+"Baggins") | `user:frodo` (bio) |
| 2 | galadriel | "Sono Galadriel, la compagna di Frodo, e amo leggere." | **2**: relationship / loves reading | `user:galadriel` (bio/preference) |
| 3 | bilbo | "Sono Bilbo, il padre di Frodo; tutti mi chiamano nonno Bruno." | **2**: relationship / nickname | `user:bilbo` (bio) |
| 4 | gollum | "Ho otto anni e faccio karate il lunedì e il giovedì." | **2**: age / karate schedule | `user:gollum` (bio) |

### 2 — Preferences

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 5 | frodo | "Mi piace il Big Mac con le patatine e preferisco parlare senza giri di parole." | **2**: food / communication style | `user:frodo` (preference) |
| 6 | galadriel | "A Matteo non piace il pesto ma adora il McDonald's." | **2** (about Matteo) | `user:gollum` — cross-user via alias Matteo |

### 3 — Family plans, presence, kids' school (`group:famiglia`)

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 7 | frodo | "Sabato cena di famiglia con i Brandibuck da noi, e domenica andiamo dai nonni." | **2**: Sat dinner / Sun grandparents | `group:famiglia` (dates resolved) |
| 8 | galadriel | "Domani lavoro solo il pomeriggio, così prendo io Matteo a karate." | **2**: presence / pickup | `group:famiglia` |
| 9 | frodo | "I compiti di Matteo sono per lunedì e giovedì ha la gita scolastica." | **2**: homework Mon / trip Thu | `group:famiglia` (kids' school) |

### 4 — Shopping list — accumulates across senders (emergence substrate)

These deliberately pile facts onto one topic from **different** people, so a
later emergence test can check whether a dedicated "lista spesa" sub-wiki gets
proposed.

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 10 | galadriel | "Oggi ho fatto la spesa, ho preso latte, formaggio, salame e pane, poi ho portato Matteo a karate." | **5**: latte / formaggio / salame / pane / Matteo a karate | groceries + karate → `group:famiglia` — **THE v2.6 regression case** (was 1) |
| 11 | frodo | "Mettete caffè e zucchero nella lista della spesa, sono finiti." | **2**: caffè / zucchero | `group:famiglia` (shared list) |
| 12 | gollum | "Mamma sono finiti i biscotti!" | **1**: biscotti | `group:famiglia` (multi-sender convergence on the list) |

### 5 — House rules

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 13 | frodo | "In casa si cena alle 20 e non si fuma." | **2**: meal time / no-smoking | `group:famiglia` |

### 6 — Episodes (one shared, one strictly personal — the scope control)

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 14 | galadriel | "Ieri Matteo ha perso il primo dentino." | **1** (about Matteo) | `group:famiglia` (shared kid episode) |
| 15 | frodo | "Oggi ho corretto un brutto bug di lumen al lavoro." | **1** | `user:frodo` — **must stay private** (the family scope explicitly excludes this exact case) |

### 7 — Cross-group + public

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 16 | frodo | "Con Bilbo sabato andiamo a pesca." | **1** | `group:amici` (frodo+bilbo) — invisible to famiglia |
| 17 | frodo | "Informazione pubblica, visibile a chiunque: la Martinelli è in Via dei Platani 4 a Ferrara." | **1** | `global` (public-fact cue) |

### 8 — Atomization edge cases (the split contract, inline)

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 18 | frodo | "Domani ho il dentista alle 9 e il nonno Bruno ha cambiato medico." | **2**, different owners | frodo → `user:frodo` (or famiglia presence); Bruno → `user:bilbo` (cross-user) |
| 19 | frodo | "Ciao Sam! tutto bene? grazie mille." | **0** (skip) | — (not-memorable → empty array) |

### 9 — More from the real household (richer multi-fact)

Reconstructed from the compiled samvise memory — denser run-ons that stress the
splitter and add coverage (commissions list, work travel, health, the kid's full
schedule, house gear).

| # | as | message (IT) | expect: split | expect: scope |
|---|---|---|---|---|
| 20 | galadriel | "Stasera quando passi portami dei cotton fioc, un cuscinetto, un asciugamano intimo e dello yogurt senza lattosio." | **4**: cotton fioc / cuscinetto / asciugamano / yogurt | `group:famiglia` (commissions) |
| 21 | frodo | "Dal 9 all'11 giugno sarò a Francoforte per il Samsung Tizen Partner Summit." | **1** (plan; dates resolved) | `group:famiglia` (presence) or `user:frodo` |
| 22 | galadriel | "Sono celiaca e intollerante al lattosio, e prendo il Gaviscon mezz'ora dopo pranzo senza acqua." | **3**: celiaca / intollerante lattosio / Gaviscon | `user:galadriel` |
| 23 | frodo | "Matteo ha karate il lunedì e il giovedì, breakdance il mercoledì e il venerdì la lezione online su Kodland." | **3-4** (one per activity) | `group:famiglia` (kids' activities) |
| 24 | galadriel | "Sono incinta del quinto mese, la bambina sta bene, ma ho la pressione un po' alta e forse mi ricoverano." | **3**: pregnant 5mo / baby fine / high blood pressure→possible admission | `user:galadriel` + `group:famiglia` |
| 25 | frodo | "La macchina del caffè si chiama Kamira, il robot dei pavimenti è Willie, e il Ficus Grande va annaffiato diversamente dalle altre piante." | **3**: Kamira / Willie / Ficus | `group:famiglia` (house gear) |

## Add your own (the human part)

Drop realistic lines here — the messier the better. Same columns. Put the split
you expect, or leave it blank and I'll record what the model does and we judge
it together.

| # | as | message (IT) | expect: split | expect: scope | notes |
|---|---|---|---|---|---|
| U1 |  |  |  |  |  |
| U2 |  |  |  |  |  |
| U3 |  |  |  |  |  |
| U4 |  |  |  |  |  |

## State this corpus leaves (baseline for downstream tests)

After ingesting the corpus + a `light` dream (promote + compile) + a `full` REM
(reorg), the memory should hold roughly:

- **Person wikis** — frodo (bio + Martinelli/Ferrara job, Big Mac, the private
  lumen-bug episode), galadriel (bio, loves books), gollum (bio, karate
  Mon/Thu, McDonald's / no-pesto, lost tooth), bilbo (bio).
- **`famiglia` group wiki** — a **shopping list with ~7 items** (latte,
  formaggio, salame, pane, caffè, zucchero, biscotti) added by three different
  senders → the **emergence substrate**; plus plans (Sat dinner, Sun
  grandparents), presence/pickup, kids' school (homework, trip), rules (cena
  alle 20, no-smoking), the karate run.
- **`amici` group wiki** — the fishing trip (invisible to famiglia).
- **A `global` fact** — the Martinelli address.

Downstream corpora reference this baseline: e.g. an emergence test asks "does
the ~7-item famiglia shopping topic get proposed as its own sub-wiki?", a REM
test exercises dedup/reorg over the accumulated facts.

## After the run

Per line I record the **observed** split (count + bodies) and the owner each
fact landed under; I flag any collapse (N→1), over-split, or mis-route, and note
where the LLM was borderline (probabilistic — a re-run gauges stability). Pass
bar for the v2.6 fix: **every multi-fact line splits into several atomic facts,
never one** (line 10 is the canonical check). Findings fold back into
[`ingest-pipeline.md`](../../docs/design-notes/ingest-pipeline.md)
and the roadmap P1 status, and the run is committed alongside this file.

### First run (2026-06-01) — PASS

First live run on `gemini-3-flash-preview` (standard-consumer dogfood). **All 25
lines split as intended**, no collapse, no over-split:

- **Line 10** (canonical regression) → **5** facts (`latte / formaggio / salame /
  pane / Matteo a karate`), all `group:famiglia`.
- **Skip** (line 19) → **0**. The welcome-primer profile (a 6-field run-on) → **8**.
- **Per-fact owner** held inside a single message (line 4 age→`user:gollum` /
  karate→`group:famiglia`; line 18 dentist→`user:frodo` / Bruno→`user:bilbo`;
  line 24 → 4 facts, 2 private + 2 family). Cross-user aliases resolved
  (`Matteo`→gollum, `nonno Bruno`→bilbo); the private bug (line 15) stayed
  `user:frodo`; the public cue (line 17) → `global`.
- A light dream compiled the 60 facts into **prose** and topic pages emerged
  (`spesa_famiglia`, `sport_matteo`, …), inline markers preserved on all but 2.

Variances / findings (not atomization failures): line 24 gave 4 vs an estimated 3
(a finer split); line 16 routed to `group:famiglia` rather than `group:amici` (a
scope judgement). One fact stayed buffered on a transient `bge-m3` `NaN` embedding.
The reviewer flagged `missing_acl_markers=2` — **not** the hub pages (verified:
hubs have no `primary_facts`), but **2 non-global facts whose protective ACL
marker the Cronista dropped** during compile — a compile-fidelity finding to chase
(separate from ingest atomization), not benign.

⚠️ **Setup gotcha that cost the first attempt:** a stale `work/prompts/ingest.md`
(v2.4) override shadowed the bundled v2.6 and made the primer collapse to 1 — the
reset now wipes `./work/prompts` (see [`instruction.md`](instruction.md) §Reset).
