---
name: smart-consumer
version: 1.18.0
description: "Project-bound mode for smart consumers: authoritative management of a project's smart wiki via wiki_admin_push/pull (whole, narrowed by paths, or shape-only) + project signposts (the description is `_meta.scope`, the diary is `wiki_admin_push`'s `activity` field — the server writes both, so the user's standard memory knows the project exists) + _briefing.md lifecycle + cooperative lease + graceful degradation on token revoke. First connect is NOT here — it lives in smart-onboarding, fetched when the server volunteers first_connect.hint. Smart wikis are markerless and content-indexed — the consumer writes plain markdown freely (create / edit / move / rename / delete pages), exactly the way this repo's engineering wiki is maintained; the ACL is wiki-level in _meta (no per-fragment markers or ACL — those are the pillar of standard memory wikis only). Superset (group 17): the user↔agent conversation ALSO runs the standard personal-memory pipeline via wiki_ingest_message, joined to the project wiki by provenance links (authored_refs), with a per-message router (drop / personal-fact→standard / document-import / project-wiki / your-operational-wiki). Auto recall+capture, never dump everything into the user's standard memory."
depends_on: ["core"]
applies_to:
  consumer_class: smart
  cwd_state: present
status: implemented
---

# mwe-mcp / smart-consumer skill

This skill is for **smart consumers** (Claude Code, Cowork, Codex —
agents that bring their own subscription LLM) working inside a
project directory that has, or will get, a `.mwe/state.json` marker.
It defines how to manage the project's companion-wiki authoritatively
without going through mwe-mcp's server-side LLM (the "double-bill"
that motivated the entire companion-wiki design).

## When this skill applies

The dispatcher in `core` loads this skill when **both**:

- Your JWT carries `consumer_class: smart` and a `consumer_id` claim.
- The current working directory contains `.mwe/state.json`, **or**
  the user explicitly asks to bootstrap a companion-wiki for the
  current directory.

If only the first holds (smart consumer, no cwd marker), the
dispatcher loads `core-globalmemory` instead and you operate in
transversal recall mode — no companion-wiki, no `wiki_admin_*`.

## The smart-consumer contract in one paragraph

A companion-wiki is **owned by the user** (`owner_user = sender_id`)
and **administered by smart consumers of that user**. You — the smart
consumer — push markdown pages to it via `wiki_admin_push` and pull the
authoritative state back via `wiki_admin_pull`. `_briefing.md` is your
**inbox**: others notify *you* there (`wiki_admin_notify`), and you
administer it yourself with an ordinary push — the server refuses a
smart consumer notifying its own wiki (`403
smart_does_not_notify_own_wiki`), because writing your own inbox is a
write, not a message. mwe-mcp's REM cycle skips all
write-jobs on companion-wikis (no auto-promote, no auto-archive, no
hub-writer), runs read-jobs (recall pre-indexing, dedup source), and
adds two notify-only sub-jobs (Briefing dispatcher + Backlink
reciprocity) that drop items into `_briefing.md`.

## The conversation also feeds personal memory (the superset)

Authoring the project wiki is **not** the whole job. A smart consumer
is a **superset** of a standard one: alongside the `wiki_admin_*`
management of the project wiki, the user↔agent **conversation** runs
the **standard personal-memory pipeline** via `wiki_ingest_message` —
exactly the per-turn passthrough a standard consumer uses (see
[`standard-conversational`](standard-conversational.md) for the wire
shape). The two write paths are joined by **links, not duplicated
detail**: project detail that already lives in the project wiki is
*referenced* from personal memory, not re-stored.

This matters because otherwise the conversation evaporates: an
appointment the user mentions while you code, a stable preference, who
someone is — none of that belongs in the project wiki, and without the
ingest path it would never reach personal memory. `wiki_ingest_message`
never lands in the project (smart) wiki — the server filters smart
wikis out of ingest routing, so a conversational turn routes only to
the user's standard personal wiki. You author the project wiki; the
server's `ingest` slot maintains personal memory.

### Message routing — your gross-route judgement per message

You decide, per message, where it goes. The set is small and
**domain-neutral** (coding, research, personal life fall out of the
same routes — there is no coding-specific schema):

| Route | When | Where it goes |
|---|---|---|
| **drop** | ephemeral operative instructions ("change this function", "re-run the tests", "fix that lint") | nowhere — not memory, no server call |
| **personal fact** | durable non-project data: a preference, who someone is, an **appointment**, a cross-project way the user works | `wiki_ingest_message` → standard personal memory; a dated one gets a validity window → the due-soon slot |
| **document-import + link** | a long pasted body the user **explicitly** asks to keep whole ("remember this document") | `wiki_ingest_external` (document-import) → its own page + a pointer, **not** atomised into facts |
| **project-wiki + link** | durable project knowledge / **decisions** (not ops) | you author the project wiki via `wiki_admin_push`; personal memory keeps a terse digest + a provenance link (see below). Project *decisions* go to the project wiki's log, never dropped |
| **your operational wiki** | your own general working notes, your **behaviour rules**, the `conversations.md` log — not the user's facts, not project knowledge | `wiki_admin_push` into the operational wiki forged for you at sign-in (if you own one — see `core`) |

**Division of labour.** The **gross route** ("is this memory at all? an
op to drop? a personal fact, a document, or project content?") is
**yours** — you know whether you are mid-coding-task or just chatting.
The **fine work** (extract the appointment, resolve its date, dedup
against what is already stored, build the page) is **the server's**:
once you route a turn to `wiki_ingest_message`, the `ingest` classifier
does it with operator knowledge (group scopes, known users, the
sender's `rules.md`) you do not have.

**Length is never the fact-gate.** "Don't ingest long messages" means
*don't store the long body verbatim* — **not** *don't look inside it*.
A dentist appointment can hide in a pasted email; gating extraction on
length drops it. So length decides only whether to **store the body**
(→ document-import, on explicit request); durable facts inside are
**always** ingested. When in doubt between "personal fact" and
"document-import", route to `wiki_ingest_message` — the classifier
extracts the durable facts and you have lost nothing.

**The within-conversation split is your judgement, not a schema.** One
work conversation carries **both** project-technical detail (→ project
wiki, linked) **and** genuinely personal / cross-project facts (who the
user is, preferences, the dentist) that stay as **full personal facts**.
Decide it the same way you already decide what to author into the
project wiki — there is no server-side guard splitting them.

### Provenance — echo `authored_refs` so personal memory links, not duplicates

After you push project content with `wiki_admin_push`, its response
carries `authored_refs` — one plain wikilink `[[wiki_id/page]]` per page
you wrote. When the **same turn** also warrants a personal-memory note
about that work, pass those refs into the next `wiki_ingest_message` as
`metadata.authored_refs`:

```
out  = wiki_admin_push(wiki_id, mode="upsert", pages=[{path:"modules/auth.md", content: ...}])
# out.authored_refs == ["[[<wiki_id>/modules/auth]]"]

wiki_ingest_message(
    text     = "<the user's message for this turn>",
    metadata = { authored_refs: out.authored_refs },
)
```

Personal memory then records a **terse digest that links** to the
project page ("reworked the MFA flow → `[[<wiki_id>/modules/auth]]`")
instead of duplicating the detail. The link is a real navigable door:
recall-as-navigation follows it, and REM's backlink-reciprocity
detector keeps the inverse honest. Carry the refs only when the turn
genuinely produced a personal-memory note worth linking — an op you
dropped needs none.

## smart_bootstrap — resume the project

Call the **MCP tool `smart_bootstrap`** (family K) at session start; it
bundles the wiki landscape + the briefing inbox into one call. The Claude
Code session-start nudge (served at `/connect/hooks/claude-code.json`)
reminds you to, but **you** make the call over your own connection (the
hook holds no token and calls nothing).

Pass the project's `project_id` — the recipe is in
[`core`](core.md) — and the server answers the only question that
matters at connect: does this project already have memory?

```
fn resume(cwd):
    project_id = state.project_id if .mwe/state.json else derive_project_id(cwd)
    snapshot   = smart_bootstrap(project_id = project_id)
    # snapshot.first_connect = { project_id, wiki_id, wiki_found, hint }
    # snapshot.smart_wikis[*] = { wiki_id, wiki_type, title, slug, project_id,
    #     matches_project_id, matches_project_hint, is_self, last_op_log_id,
    #     last_op_log_ts, briefing_counts, recent_briefing[...] }

    if not snapshot.first_connect.wiki_found:
        # No memory for this project yet. That is the ONE case this skill
        # does not handle: load `smart-onboarding` and follow it.
        return handoff_to("smart-onboarding")

    wiki_id = snapshot.first_connect.wiki_id
    me      = the entry with matches_project_id

    # 1. Surface unread briefing items — already in the snapshot.
    surface_to_user(me.recent_briefing)     # already filtered to processed_at IS NULL;
                                            # briefing_counts has the per-kind totals

    # 2. Reconcile the local mirror with the server.
    state = read(".mwe/state.json")                       # absent on a fresh clone
    if state and state.last_op_log_head < me.last_op_log_id:
        pull = wiki_admin_pull(wiki_id = wiki_id)         # add paths=[…] to narrow it
        for page in local_edits_not_in(pull):
            wiki_admin_push(wiki_id, mode="upsert", pages=[page])
    return wiki_id
```

Three things to internalise:

- **A wiki on mwe with no local `.mwe/state.json` is a sync, never a second
  wiki.** It was bootstrapped on another machine, or the local `.mwe/` was
  wiped: pull, write the state file, reconcile, resume.
- **The `project_id` is stable**, which is what makes that convergence
  work: it comes from the VCS origin plus the repo-relative path of the
  project root, so renaming the local checkout changes nothing and two
  laptops with the same clone land on the same wiki.
- **`op_log_id` is a global counter, not per-wiki.** It jumps between two
  of your own pushes because other wikis wrote in between. Stamp back
  whatever the last response returned; never compute the value you expect.

## Markerless, content-indexed — write the wiki like local files

A smart wiki is **markerless and content-indexed**. You **own** the
project wiki and write **plain markdown freely** — create, edit, move,
rename, and delete pages — exactly the way the engineering wiki of
*this* repo is maintained. There is no server-side LLM, no style engine,
and no custom-type registration: the bytes you push are the bytes
stored.

- **No per-fragment markers, no per-fragment ACL.** Smart wikis carry
  **no** `{{f=…}}` markers and no per-fragment ACL — those are the
  pillar of **standard memory wikis** only. The ACL here is
  **wiki-level**, in `_meta`: the owner is your user (`owner_user`),
  and the wiki is **private until the owner shares it** from the
  dashboard (`shared_with`). You never stamp ACL onto a paragraph.
- **Recall indexes the content by section.** On each push the touched
  pages are re-chunked into **heading-delimited sections**, embedded,
  and the wiki's rows are dropped-and-reinserted. So the only
  guard-rails you must respect are: **keep pages
  reasonably structured with markdown headings** (each
  heading-delimited section becomes a recallable unit), and **leave
  `_meta` and `_captures` as-is** (the server owns them — a malformed
  `_meta` is rejected on push). Otherwise structure the wiki however
  you like — keep a `roadmap.md`, a `planning/` folder, whatever fits
  the project.

Those conventions are documented in
the smart-wikis design note and, for codebases, in the
`smart-codebase` skill — read them once and conform.

## `conversations.md` — one dated entry per working session

Your operational wiki holds a `conversations.md`: newest first, **one dated
entry per working session**. It is what lets the next session start where
this one stopped instead of reconstructing it from the code, and it is
yours to keep — nobody will ask you for it.

**When to write it.** Not "as you go": that names no moment, and a rule
without a moment does not fire. Write the entry when the session has
produced something durable **and** you reach a natural close — a piece of
work finished, a decision taken, or the user signalling the end. Do not
wait to be asked, and do not save it for a tidy final turn that may never
come: a session that ends unwritten is one the next session has to
reconstruct. When in doubt, write it; a short entry costs one push.

**What goes in it:** what you built or decided and why, and — the part that
pays for itself — **what you got wrong**, stated plainly, so the next
session does not repeat it. **What stays out:** facts about the *user*
(those go to `wiki_ingest_message`) and project knowledge or project
decisions (those go to the project wiki). This page is your own working
memory, not a duplicate of either.

## Append-only log pages — keep them bounded, rotate by period

An append-only chronological page — your operational wiki's
`conversations.md`, or a project wiki's decision/change log — grows
without bound. Two things go wrong for a smart wiki when it does:

- **Recall granularity collapses.** Every push re-chunks the page into
  heading-delimited sections (above). A monolithic log with few headings
  embeds as a handful of giant sections, so recall can't home in on the
  one entry that matters — the whole slab surfaces or nothing does.
- **It outgrows a single read.** Past a point the page no longer fits in
  one `read_local`: you must reassemble it from offset-chunks to edit or
  re-push it, and hand-reassembly is exactly where byte-drift creeps into
  a "faithful" copy. Nothing warns you — **there is no server-side size
  cap** (mwe stores a 300 KB page happily); the ceiling you hit is your
  own file-read tool's.

**Rule.** Keep any single log/changelog page **bounded** — a live page
should sit comfortably inside one read (well under your read tool's
ceiling) and split into a dozen-ish sections, not scores of them. When a
push would take it past that, **rotate by period**: the live page
(`conversations.md`, `log.md`) keeps only the current period's entries;
older entries move into a dated, append-only archive
(`conversations.<YYYY-Qn>.md`, `log.<YYYY>.md`). Push the trimmed live
page and the archive **together** in one `wiki_admin_push mode: upsert`
so the move is atomic — the same idiom as `_briefing.md →
_briefing.archive.md`.

A period boundary (quarter, year) is the natural cut for a chronological
log; if one period is itself too big, cut finer. Archived pages stay
recallable — you are bounding each page's mass, not discarding history.
The codebase specialisation (a project wiki's `log.md` — first-class
content, structured by date and paged into archives, never dropped) is in
`smart-codebase`.

## Nothing tidies a smart wiki but you

The nightly REM cycle runs **no write-jobs** on a smart wiki (above): no dedup
merge, no auto-promote, no page merge, no compiler rewrite, no forgetting.
That is deliberate — you own these bytes and a server-side rewrite would fight
you. Internalise the consequence: **a standard memory wiki gets a janitor every
night; yours has you.** Left alone it only accretes — the same conclusion
restated five times across five sessions, decisions that stopped being true
sitting next to the ones that replaced them, scaffolding notes nobody will read
again. That is not a tidiness problem: recall returns the loudest match, so a
corpus of near-duplicates is a corpus that answers with noise.

Do the upkeep **where you already are**, never on a timer you cannot keep. You
pull at session start and push as you work; the cheap moment is the push you
were making anyway, on the pages that push touches. Each pass, on those pages
only:

- **Fold restatements.** Three entries saying one thing become one entry at the
  clearest wording. Two near-identical entries about **different** people,
  projects or occasions are *not* duplicates — that distinction is the whole
  point of a memory, and collapsing it loses the very thing that made the two
  worth keeping.
- **Retire what stopped being true.** A superseded decision is not history to
  preserve in place: state the current truth and let the dated log page carry
  the "we used to do X" if it earns its keep. Pages describe the present; the
  log describes the past.
- **Re-shape when the content moved.** Split a page that grew a second subject,
  merge two that turned out to be one, rename what is misnamed. You may move
  pages freely — there is no server-side plan to keep in sync.
- **Drop the scaffolding** — the note whose only value was getting you through
  one task that is now done.

Keep the pass **bounded**: minutes on this session's pages, not an audit of the
whole wiki. A wiki you rewrite wholesale every session is one whose history
nobody can trust; a wiki you never revisit is one that quietly rots. The same
duty covers a **project** wiki you own — same absence of a janitor, same rule.

## Day-to-day editing loop

After bootstrap, the loop is `local edit → wiki_admin_push mode:
upsert`. All local edits happen under **`state.local_wiki_root`** — the
local markdown mirror, `.mwe/wiki/` by default but the ingested
directory (e.g. `wiki/`) when you bootstrapped over an existing wiki in
place. Three patterns:

### Single-page edit

```
# Local edit happens under state.local_wiki_root, e.g.
#   <local_wiki_root>/modules/auth.md   (default .mwe/wiki/, or the ingested dir)
new_body = read_local(state.local_wiki_root + "/modules/auth.md")

wiki_admin_push(
    wiki_id = state.wiki_id,
    mode = "upsert",
    pages = [{"path": "modules/auth.md", "content": new_body}],
    expected_op_log_head = state.last_op_log_head,  # optimistic concurrency
)

# On success: bump state.last_op_log_head to the new op_log_id.
# On 409 conflicting_op_log_head: pull → re-diff → re-push.
```

### Bulk edit guarded by a cooperative lease

When you're about to push **many pages in sequence** (e.g. you just
restructured `modules/` into 4 new files plus 2 deletions), acquire a
cooperative lease first. The lease tells other smart consumers of the
same user "I am authoritative now; please defer your upserts". It is
**advisory**, not a syntactic mutex: enforcement is server-side only
on the `upsert` mode of `wiki_admin_push`, and only when held by a
foreign `(sender_id, consumer_id)` pair.

```
lease = wiki_admin_lease_acquire(
    wiki_id = state.wiki_id,
    ttl_seconds = 60,     # default 60s; cap 300s — keep it short
)
# lease.lease_id, lease.expires_at

try:
    for page in batch:
        wiki_admin_push(
            wiki_id = state.wiki_id,
            mode = "upsert",
            pages = [page],
        )
finally:
    wiki_admin_lease_release(lease_id = lease.lease_id)
```

On `423 wiki_locked_by_lease` from a competing push: back off and
retry after `lease.expires_at` (the wire error includes the holder's
`consumer_id` + `sender_id` + `expires_at`), or surface to the user
that another device is editing.

Re-acquiring from the same `(sender_id, consumer_id)` extends the
existing lease in place — no need to release first when you want to
prolong your hold. Stale leases (laptop crashed mid-push, network
partition) are swept by REM's `lease_expirer` sub-job with a 1h
grace + 7d retention.

### Full rewrite

To replace the whole wiki after a local regeneration, push the new
page set with `mode: upsert` and list every now-removed page in the
push's `delete` paths. There is no single "replace everything" mode —
`upsert` (+ deletes) is the only edit mode beside `create`. Each op is
recorded in the op-log; the dashboard's `/wikis/<id>/op-log` exposes a
one-click revert window.

### Read the push response — it tells you things you cannot see

Three fields carry information you would otherwise never get:

- **`warnings[]`** — a page you just wrote has blocks too long for the
  index to keep whole, so they are cut mid-sentence and several sections
  end up under the same heading with different content. The line is
  written in plain language: relay it, and offer the repair from
  [`smart-onboarding`](smart-onboarding.md) ("Repairing a page"). This is
  how a page that grew across sessions gets caught — nobody notices
  otherwise, and the usual fix is a blank line, not surgery.
- **`signpost_hint`** — see the next section.
- **`section_indexing`** — `"queued"` means the sections (and their
  embeddings) are still being built when the ack arrives, so a recall
  immediately after a big push can lag by the queue depth. Not an error;
  just do not test recall in the same breath as the push.

**Reading back cheaply.** `wiki_admin_pull` takes `paths: [...]` to
narrow to the pages you care about, and `shape: true` to get *how each
page will retrieve* — sections, over-long blocks, and a per-page note —
without pulling a single byte of content through your context.

## Project signposts — make the user's own agent aware the project exists

The user's everyday agent (Telegram, chat, whatever they talk to about
their life) recalls **facts**, never your project pages. That is
deliberate: a personal conversation must not be buried under project
documentation. The consequence is blunt — unless the user *names* the
project, their standard memory has no idea it exists, and cannot connect
a question to it.

A **signpost** fixes that. It is a short line you write into the user's
own memory saying that this project exists, and what happened lately.
When a signpost surfaces in an ordinary turn, the engine opens this
project's documentation for that turn. So the signpost is what makes an
unnamed question reach your wiki at all.

> **A signpost is a pointer, not a record.** It is not where the work is
> written down — that is what this wiki is for. Do not try to make it
> complete, and never treat it as the place to preserve detail. Its only
> job is to make the memory realise there is something here worth
> looking at.

### How the two halves get written — neither is a call you make

**The description is a property of your wiki, not an act.** Put it in
`_meta.md` as `scope:` — one short non-technical sentence — and push. The
server mirrors it into its registry and writes it into the user's memory
itself, on every sweep. Edit the line and it follows; delete it and the
signpost is retired. There is nothing to call, and nothing to remember:
a project with no `scope:` is simply a project with no door, visibly.

```yaml
# _meta.md
scope: "The system that runs the digital signs in the shops: it decides what to show on each screen and when to refresh it."
```

**The diary line rides the push that carried the work.** Pass `activity`
on `wiki_admin_push` itself — one sentence about what this push was
about — and the server writes today's diary entry. No second call. The
ack tells you what it did under `diary` (`"written"`, `"unchanged"`, or
`null` when you passed nothing).

```
wiki_admin_push(
    mode = "upsert",
    wiki_id = state.wiki_id,
    pages = [...],
    activity = "Fixed a fault that left old content sitting on the screens even after an update.",
)
```

Omit `activity` for a push that carried no real work — a typo fix, a
reformat. Silence is a legitimate answer; a diary of nothing is worse
than a short diary.

> **Why it changed.** Both halves used to be a separate
> `wiki_admin_signpost` call, prompted by a hint on every push. Counted
> across the whole recorded window, four projects on the maintainer's own
> deployment ever got a description written — the largest undescribed
> wiki had 1 477 pages of documentation and no way for its owner's agent
> to know it existed. Nothing was lost to conflicts; the call simply was
> not made. So the description became a property, and the diary became a
> field on a call you are already sending.

### Tone — this is where it goes wrong

You are writing for someone who has **never seen the code** and is not
reading a changelog. Write it the way you would say it out loud to the
user's non-technical partner. Write in the user's own language.

| | |
|---|---|
| ✅ good description | «The system that runs the digital signs in the shops: it decides what to show on each screen and when to refresh it.» |
| ❌ bad description | «Angular/NestJS monorepo with headless rendering workers, Tizen players and a Socket.IO sync pipeline.» |
| ✅ good activity | «Fixed a fault that left old content sitting on the screens even after an update.» |
| ❌ bad activity | «Fixed retry exponential-backoff in the job dispatcher (PR #214), refactored the state reducer.» |

The bad ones are not wrong — they are unusable. A signpost written in
jargon signposts nothing, because the agent reading it cannot tell
whether the user's question has anything to do with it.

### The rules the server enforces

- **description** (`_meta.scope`) — max 400 characters, one per project.
  Editing it replaces the old one; removing it retires the signpost.
- **activity** (`wiki_admin_push`'s `activity`) — max 250 characters, one
  per day. Pushing twice in a day replaces that day's line. The server
  prefixes the date and the project name; your text carries only what
  happened.
- Over the cap ⇒ the **push is refused** with the measured length, never
  silently truncated. Rewrite shorter; do not retry the same text.
- The two live on **two pages** in the user's memory — the door signs and
  the diary — because one is rebuilt from your `_meta.md` on every sweep
  and the other accumulates. You write to neither: the server does.
- Only days inside a rolling **5-day window** are kept — older lines drop
  off on their own. Do not try to keep a history here.
- Only the **owner** of the project wiki can signpost it, and only for a
  smart wiki.

## `_briefing.md` lifecycle

`_briefing.md` is a single markdown file at the root of every
companion-wiki. It is the **inbox** through which the rest of the
mwe-mcp ecosystem talks to the smart consumer: REM's Briefing
dispatcher drops stale-draft observations there, the Backlink
reciprocity detector drops missing-inverse-link alerts there, openclaw
forwards user observations from chat there, and shared-with team
members notify there too.

### Read at session start

`smart_bootstrap` pulls `_briefing.md` from the server. Parse its
`## Unread` section (or whichever convention the bundled type uses —
the bundled `wiki-companion` uses headings of the form
`## From <source> @ <ts>` with items marked `unread:`). Surface the
unread items to the user **before** discussing any other topic:

> *"3 new briefing notes since the last session:*
>
> *1. (REM, yesterday 23:14) `modules/auth.md` links to*
>    *`runbooks/mfa-onboarding.md`, but that runbook does not link back.*
>    *Shall I propagate the backlink?*
>
> *2. (openclaw, 2026-05-24 18:02) Frodo via Telegram: "note this down:*
>    *document the recovery codes in the MFA flow."*
>
> *3. (bob @lnprint-devs, 2026-05-25 09:30) "the MFA flow I updated in*
>    *`src/auth/mfa.rs` is not reflected in `modules/auth.md` yet."*
>
> *Do you want to take any of these on now?"*

### Archive on action

When the user acknowledges or acts on an item, move it from
`_briefing.md` to `_briefing.archive.md` (append-only) with a
`## Resolved @ <ts>` heading and the resolution note. Push both
files in a single `wiki_admin_push mode: upsert` so the move is
atomic.

### Leave a note for your next session

If the session ends with a half-finished thought worth revisiting —
"next time check whether dedup of `decisions/2025-q4-*` helps",
"re-check the licence pinning" — append it to your own `_briefing.md`
with an ordinary **push**, the same way you archive an item:

```
wiki_admin_push(wiki_id = state.wiki_id, mode = "upsert", pages = [
    { "path": "_briefing.md", "content": <current content + the new item> },
])
```

**Not `wiki_admin_notify`** — the server refuses a smart consumer
notifying its own smart wiki (`403 smart_does_not_notify_own_wiki`), and
it is right to: `notify` is how *others* reach your inbox, while writing
your own inbox is just a write you are already authorised to make.

## Three-layer briefing classification

Each briefing item carries a `kind` tag — `observation`, `reasoning`,
or `external` — that decides routing on REM's side. The classifier
is server-side; you receive the items already tagged in
`BriefingItem.kind`. Surface them grouped by kind when the volume
warrants it:

| `kind` | Meaning | Typical sources |
|---|---|---|
| `observation` | A factual delta the user or a peer noticed | openclaw forwards; team notifies via `shared_with` |
| `reasoning` | An inference REM made (backlink missing, dedup candidate, stale section) | Briefing dispatcher, Backlink reciprocity, dedup-source |
| `external` | A reference outside the wiki that the user wants tied in | Citations from chat, links from the dashboard |

## Citation IDs

When notifying about a specific section, populate
`wiki_admin_notify.target_cite` with a handle of the form
`wiki://<wiki_id>/<page_path>(#<heading-slug>)?`. The server validates
it via `briefing::parse_cite` and renders it in `_briefing.md` as an
Obsidian autolink. Example:

```
wiki_admin_notify(
    wiki_id = state.wiki_id,
    topic = "backlink missing",
    body = "auth.md links runbooks/mfa-onboarding but no inverse.",
    source = {"kind": "rem", "ref": "backlink_reciprocity"},
    target_cite = "wiki://" + state.wiki_id + "/modules/auth.md#mfa-flow",
)
```

The user gets a click-through from `_briefing.md` straight to the
relevant heading inside Obsidian (or the dashboard `/cite/` resolver
when it lands).

## Shared-with companion-wikis

The owner can extend read access to the companion-wiki via the
dashboard `/wikis/<id>/sharing` page, adding `user:<id>` /
`group:<id>` / `global` entries to `_meta.md` field `shared_with`.
Read-side resolution (owner → user → group → global → denied) is
handled by `mwe_core::wiki_admin::resolve_read_access`; you do not
manage ACLs from the smart consumer.

Two consequences for the smart consumer of a **non-owner** user
(team member with read access):

- `wiki_search` and `wiki_read` work normally — the wiki is visible
  in your recall surface.
- `wiki_admin_push` / `wiki_admin_pull` return
  `403 wiki_owned_by_other_user`. You **cannot** edit the wiki of
  another user even when shared with you; you can only **notify** via
  `wiki_admin_notify`, and the item lands in the owner's
  `_briefing.md` for them to triage.

## Graceful degradation on token revoke

When the operator revokes the smart consumer's JWT, mwe-mcp's auth
middleware returns `401 token_revoked` on the next `wiki_admin_*`
call — distinct from the generic `401 invalid_token`. Treat the two
paths differently:

| Wire code | Caller behaviour |
|---|---|
| `401 invalid_token` | Signature mismatch / clock skew / unknown server. Hard configuration error: surface immediately, do **not** queue local writes. |
| `401 token_revoked` | JTI in `token_blacklist`. **Keep the local mirror (`state.local_wiki_root`) intact** (no work lost), surface a "token revoked — please issue a new one" prompt to the operator, **continue local editing** until they paste a fresh token. |

When a fresh token arrives, the next `smart_bootstrap` does:

1. `wiki_admin_pull` first — absorbs any `_briefing.md` items that
   landed in the gap (REM Briefing dispatcher findings, openclaw
   forwards, team notifications via `shared_with`).
2. Diff pulled state vs the local mirror (`state.local_wiki_root`).
3. Replay queued local edits with `wiki_admin_push mode: upsert` (one
   push per page that diverged locally). Optimistic concurrency via
   `expected_op_log_head` short-circuits if a concurrent device
   pushed in the same window.

`.mwe/state.json` recommended shape during a revoke window:

```json
{
    "wiki_id": "w_lnprint_xy",
    "last_op_log_head": "ol_abc123",
    "wiki_type": "wiki-companion",
    "project_id": "ab12cd34ef56",
    "local_wiki_root": ".mwe/wiki/",
    "pending_pushes": [
        {"path": "modules/auth.md", "content_sha256": "...", "queued_at": "2026-05-25T08:14:00Z"}
    ]
}
```

The smart consumer drains `pending_pushes` on successful replay. No
automatic merge — last writer wins for concurrent edits across
devices of the same user, which is the right semantic for the
single-laptop single-token rotation case that motivated it.

## Tools used

| Family | Tool | Purpose |
|---|---|---|
| A | `wiki_ingest_message` | route the user↔agent conversation into the user's standard personal memory (the superset path); carry `metadata.authored_refs` to link a digest to a just-pushed project page |
| D | `wiki_search` | flat top-K lookup — locate the project's existing companion-wiki at bootstrap, quick one-line recall |
| D | `wiki_navigate` | **deep** recall — a navigator walks the wiki structure hop by hop (the path becomes the context) and returns the flat hits too. For a question that needs depth or to connect things across pages; pass `topics`/`owners` you know. Costs an LLM call per hop, so keep `wiki_search` for quick lookups. Smart wikis aren't funnel-navigated (read your own with `wiki_admin_pull`) |
| F | `wiki_ingest_external` | document-import: a long body the user asks to keep whole becomes its own page + pointer |
| H | `wiki_admin_push` | create + upsert pages (modes `create` / `upsert`; deletes ride the `upsert` push); response carries `authored_refs` |
| H | `wiki_admin_pull` | whole wiki, narrowed by `paths`, or `shape: true` for per-page retrieval quality without the bytes |
| H | `wiki_admin_signpost` | **superseded — do not call.** The description is `_meta.scope` and the diary is `wiki_admin_push`'s `activity` field; the server writes both. Kept only so an older consumer does not break |
| H | `wiki_admin_notify` | append an item to **someone else's** `_briefing.md`; your own inbox you write with `wiki_admin_push` |
| K | `smart_bootstrap` | the session-start landscape + `first_connect`: does this project already have memory? |
| H | `wiki_admin_lease_acquire` / `_release` | cooperative lease for bulk edits |
| I | `skill_list` / `skill_fetch` | discover and load the bundled skills |

## Anti-patterns

- ❌ **Writing *project content* through `wiki_ingest_message`.** The
  project wiki is authored only via `wiki_admin_push`; the server
  filters smart wikis out of ingest routing, so ingest can never land
  there. This is **not** a ban on `wiki_ingest_message` itself — you
  **do** call it for the user↔agent **conversation**, which routes to
  the user's standard personal wiki (see "The conversation also feeds
  personal memory"). The line is: project pages → `wiki_admin_push`;
  conversation → `wiki_ingest_message`.
- ❌ **Letting the conversation evaporate.** Authoring the project wiki
  is half the job. A turn that carries a durable personal fact (a
  preference, an appointment, who someone is) must reach personal
  memory via `wiki_ingest_message`, or it is lost the moment the
  session ends.
- ❌ **Pushing without bumping `last_op_log_head`.** You lose
  optimistic concurrency and the next pull-then-push cycle silently
  overwrites concurrent edits from another device.
- ❌ **Long-lived leases.** The cap is 300 s for a reason — a crashed
  laptop holding a 300s lease blocks every other device for up to
  300 s plus REM grace. Default 60 s suffices for any sensible batch.
- ❌ **Editing files outside your local mirror and expecting them to push.**
  The push surface accepts page bodies you provide; it does not scan
  the filesystem. Your local mirror is **`state.local_wiki_root`**
  (`.mwe/wiki/` by default, or the directory you ingested in place) — edit
  there, and pass those bodies to `wiki_admin_push`.
- ❌ **Silently auto-converting `docs/`.** Importing a project's existing
  documentation is first-connect work and it always waits for a yes — see
  [`smart-onboarding`](smart-onboarding.md). The originals are never
  renamed, moved, or deleted.
- ❌ **Ignoring `warnings[]` on a push.** It is the only moment anyone
  finds out that a page has stopped retrieving properly.
- ❌ **Ignoring `_briefing.md` at session start.** The whole point of
  the inbox is that the user sees stale items every session. If you
  bootstrap and skip the briefing read, REM's notify-only sub-jobs
  produce a write-only sink and the user never benefits from them.
- ❌ **Writing the signpost as a changelog.** Commit subjects, PR
  numbers and module names in an `activity` line make it unreadable to
  the agent it is written for, which is the only reader it has. If you
  cannot say it without naming a file, it does not belong in a signpost.
- ❌ **Ignoring `signpost_hint`.** It appears in the push response only
  when something is actually missing. Skipping it means the user's own
  agent keeps not knowing this project exists.

## Cross-references

- Bootstrap document: [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).
- Sibling skills: `core-globalmemory` (transversal mode, no cwd state),
  `smart-codebase` (codebase layout + page conventions),
  [`smart-onboarding`](smart-onboarding.md) (**first connect**: the intro,
  the faithful import, the shape report, the page-repair proposal).
- Wire-level tool spec: `docs/protocol/mcp-tools.md` family H.
- Engineering wiki: the smart-wikis design note.
- Lease design: the rem-cycle design note §"Lease expirer
  sub-job", `crates/mwe-core/src/wiki_admin_leases.rs`.
- `_meta` / frontmatter constraints: the smart-wikis design note
  and the `smart-codebase` skill.
