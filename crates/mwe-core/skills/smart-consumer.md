---
name: smart-consumer
version: 1.9.0
description: "Project-bound mode for smart consumers: authoritative management of a project's smart wiki via wiki_admin_push/pull/notify + _briefing.md lifecycle + cooperative lease + graceful degradation on token revoke. Smart wikis are markerless and content-indexed — the consumer writes plain markdown freely (create / edit / move / rename / delete pages), exactly the way this repo's engineering wiki is maintained; the ACL is wiki-level in _meta (no per-fragment markers or ACL — those are the pillar of standard memory wikis only). Superset (group 17): the user↔agent conversation ALSO runs the standard personal-memory pipeline via wiki_ingest_message, joined to the project wiki by provenance links (authored_refs), with a per-message router (drop / personal-fact→standard / document-import / project-wiki / your-operational-wiki). Auto recall+capture, never dump everything into the user's standard memory."
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
consumer — push markdown pages to it via `wiki_admin_push`, pull the
authoritative state back via `wiki_admin_pull`, and notify the
project's `_briefing.md` via `wiki_admin_notify` when you want to
leave a note for the next session. mwe-mcp's REM cycle skips all
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

## smart_bootstrap

Run at session start when `.mwe/state.json` is present in cwd, or on
the first user turn that mentions the project clearly enough to
trigger an opt-in bootstrap.

There is a dedicated **MCP tool `smart_bootstrap`** (family
K) that bundles the recall + briefing-fetch into a single call. Call it
at session start — the Claude Code session-start nudge (served at
`/connect/hooks/claude-code.json`, `SessionStart`) reminds you to, but
**you** make the call yourself over your own connection (the hook holds
no token and does not call the server).
The pseudocode below documents
the *call shape*: when the K-family tool is available, replace step
(2)/(2.a) with a single `smart_bootstrap({ project_hint: <derived
project_id or slug> })` and read `smart_wikis[0]` for the resume
branch (or `smart_wikis == []` for the initialise branch).

```
fn smart_bootstrap(cwd):
    # 1. Identify the project
    project_id = derive_project_id(cwd)
    # default derivation: sha256( normalized_vcs_origin + ":" + cwd_relpath_from_repo_root )[..16]
    # CLAUDE.md may override: "mwe-mcp: project_id=manual:<slug>"

    # 2. Look up the caller's companion-wiki landscape via the K-family tool
    snapshot = smart_bootstrap(project_hint = project_id)
    # snapshot.smart_wikis[*] = { wiki_id, wiki_type, title, slug,
    #     project_id, matches_project_hint, last_op_log_id,
    #     last_op_log_ts, briefing_counts, recent_briefing[...] }

    matches = [c for c in snapshot.smart_wikis if c.matches_project_hint]
    if matches is non-empty:
        # Branch A: resume an existing companion-wiki
        wiki_id = matches[0].wiki_id
        state = read(".mwe/state.json")          # may be absent on first session

        # 2.a Surface unread briefing items — already in the snapshot
        surface_to_user(matches[0].recent_briefing)
        # recent_briefing already filters processed_at IS NULL;
        # briefing_counts has the per-kind totals for a UI badge.

        # 2.b Reconcile local cache (if any) with server state
        if state and state.last_op_log_id < matches[0].last_op_log_id:
            pull = wiki_admin_pull(wiki_id = wiki_id)   # full pull, narrow with paths= when bandwidth matters
            diff = local_vs(pull)
            if diff has local edits:
                # Replay queued local edits — see "Day-to-day editing loop"
                for page in diff.locally_modified:
                    wiki_admin_push(wiki_id, mode="upsert", pages=[page])
        return wiki_id

    # Branch B: no companion-wiki for this project yet — initialise.
    # The smart consumer never registers a custom type and never applies
    # styles — it just writes pages directly, respecting the documented
    # _meta / frontmatter rules. `wiki_type` is a free-form tone/label
    # (use the project name, or "project"); it does NOT determine
    # smart-ness — the explicit `smart = true` flag does that.
    wiki_type = "project"

    # 2.5 Surface pre-existing CLAUDE.md documentation rules BEFORE generating
    #     any pages. Two options (adopt the mwe standard / stop so the user
    #     edits CLAUDE.md), the choice logged in
    #     .mwe/state.json.bootstrap_decisions.claude_md_doc_rules. Never
    #     silently obey or delete them. See the smart-codebase skill.
    resolve_claude_md_doc_rules(cwd)

    # 3. Seed the pages. NEVER scan or rename folders on your own — the local copy
    #    stays intact. If the user is converting pre-existing docs/ or an existing
    #    wiki, follow smart-codebase "Ingesting pre-existing docs or a wiki" (build
    #    from docs as *source*, or check-and-ingest an existing wiki — the originals
    #    are never moved). Otherwise seed a single index page.
    pages = [seed_index_page()]

    # 4. Create the companion-wiki server-side. `smart = true` is what
    #    forges a smart wiki (markerless, content-indexed); `wiki_type`
    #    is just a descriptive label.
    out = wiki_admin_push(
        project_id = project_id,
        wiki_type = wiki_type,
        smart = true,
        mode = "create",
        pages = pages,
    )

    # 5. Persist local state
    write(".mwe/state.json", {
        "wiki_id": out.wiki_id,
        "last_op_log_head": out.op_log_id,
        "wiki_type": wiki_type,
        "project_id": project_id,
        "checksums": {page.path: sha256(page.content) for page in pages},
    })
    return out.wiki_id
```

Two things to internalise:

- **The bootstrap is interactive, and never touches the local copy.** You
  do not scan or rename a project's folders on your own — `docs/` and any
  existing wiki stay exactly as they are; you read them as *source* (see the
  `smart-codebase` "Ingesting pre-existing docs or a wiki" flow). Before
  generating, scan `CLAUDE.md` **and `AGENTS.md`** for documentation-style
  rules and resolve them with the user (two options — adopt the mwe standard,
  or stop so they edit the file), logging the choice in `.mwe/state.json`.
- **Bootstrap is never automatic, and it splits — never fork a duplicate.**
  The first durable-project-knowledge write-moment *is* the deferred bootstrap.
  When this cwd is not yet bootstrapped, derive the `project_id` and look it up
  *even without* a local `.mwe/state.json`:
  - a wiki already exists for it **on mwe** (bootstrapped on another machine) →
    **propose a sync** (pull → reconcile), never a second wiki;
  - none on mwe, but the repo has an **existing local wiki/docs** the user never
    ingested → **propose ingesting that** (check-compat → push, or build from
    docs), so you write into the real wiki, not a parallel one;
  - nothing anywhere → **propose creating** a new project wiki.
  All are proposals — never write silently, never create a duplicate. If the user
  declines to ingest, keep the wiki **local-only** (edit the files, don't push —
  it just will not be in mwe recall) or skip; park pending knowledge in your
  operational wiki meanwhile so it is not lost. See
  [`core-globalmemory`](core-globalmemory.md) "Cwd discrimination".
- **The `project_id` is stable.** It comes from the VCS origin + the
  repo-relative path of the project root. Renaming the local
  checkout doesn't change it; cloning the same repo on a different
  laptop produces the same id, so two devices of the same user
  converge on the same companion-wiki rather than forking duplicates.

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
  guard-rails ("paletti") you must respect are: **keep pages
  reasonably structured with markdown headings** (each
  heading-delimited section becomes a recallable unit), and **leave
  `_meta` and `_captures` as-is** (the server owns them — a malformed
  `_meta` is rejected on push). Otherwise structure the wiki however
  you like — keep a `roadmap.md`, a `planning/` folder, whatever fits
  the project.

Those conventions are documented in
the smart-wikis design note and, for codebases, in the
`smart-codebase` skill — read them once and conform.

## Day-to-day editing loop

After bootstrap, the loop is `local edit → wiki_admin_push mode:
upsert`. Three patterns:

### Single-page edit

```
# Local edit happens in .mwe/wiki/modules/auth.md (or wherever)
new_body = read_local(".mwe/wiki/modules/auth.md")

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

> *"3 nuovi appunti sul briefing dall'ultima sessione:*
>
> *1. (REM, ieri 23:14) Il modulo `modules/auth.md` ha un wikilink a*
>    *`runbooks/mfa-onboarding.md` ma quel runbook non rilinka indietro.*
>    *Vuoi propagare il backlink?*
>
> *2. (openclaw, 2026-05-24 18:02) Frodo via Telegram: "appunta:*
>    *documentare i recovery codes nel flow MFA."*
>
> *3. (bob @lnprint-devs, 2026-05-25 09:30) "il flow MFA che ho*
>    *aggiornato in `src/auth/mfa.rs` non è ancora riflesso in*
>    *`modules/auth.md`."*
>
> *Vuoi che ci occupiamo di qualcuno di questi ora?"*

### Archive on action

When the user acknowledges or acts on an item, move it from
`_briefing.md` to `_briefing.archive.md` (append-only) with a
`## Resolved @ <ts>` heading and the resolution note. Push both
files in a single `wiki_admin_push mode: upsert` so the move is
atomic.

### Notify yourself for next time

If the current session ends with a half-finished thought worth
revisiting — "next time look at whether dedup of `decisions/2025-q4-*`
helps", "remind me to check the licence pinning when I'm back" — call
`wiki_admin_notify` to drop it into your own `_briefing.md`. The next
`smart_bootstrap` surfaces it like any other source.

```
wiki_admin_notify(
    wiki_id = state.wiki_id,
    topic = "follow-up dedup decisions/2025-q4",
    body = "We deferred the cluster review. Decision pages have grown to 18; revisit at next session.",
    source = {"kind": "consumer", "ref": "self/cc-laptop"},
)
```

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
| `401 token_revoked` | JTI in `token_blacklist`. **Keep the local `.mwe/wiki/` cache intact** (no work lost), surface a "token revoked — please issue a new one" prompt to the operator, **continue local editing** until they paste a fresh token. |

When a fresh token arrives, the next `smart_bootstrap` does:

1. `wiki_admin_pull` first — absorbs any `_briefing.md` items that
   landed in the gap (REM Briefing dispatcher findings, openclaw
   forwards, team notifications via `shared_with`).
2. Diff pulled state vs local `.mwe/wiki/`.
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
| H | `wiki_admin_pull` | full or path-narrowed pull for reconcile / revoke replay |
| H | `wiki_admin_notify` | append items to `_briefing.md` |
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
- ❌ **Editing files outside `.mwe/wiki/` and expecting them to push.**
  The push surface accepts page bodies you provide; it does not scan
  the filesystem. Treat `.mwe/wiki/` as the canonical local mirror.
- ❌ **Silently auto-converting `docs/`.** Always confirm the rename
  with the user, never `rm -rf` historical content.
- ❌ **Ignoring `_briefing.md` at session start.** The whole point of
  the inbox is that the user sees stale items every session. If you
  bootstrap and skip the briefing read, REM's notify-only sub-jobs
  produce a write-only sink and the user never benefits from them.

## Cross-references

- Bootstrap document: [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).
- Sibling skills: `core-globalmemory` (transversal mode, no cwd state),
  `smart-codebase` (codebase-specific conversion + layout
  patterns).
- Wire-level tool spec: `docs/protocol/mcp-tools.md` family H.
- Engineering wiki: the smart-wikis design note.
- Lease design: the rem-cycle design note §"Lease expirer
  sub-job", `crates/mwe-core/src/wiki_admin_leases.rs`.
- `_meta` / frontmatter constraints: the smart-wikis design note
  and the `smart-codebase` skill.
