---
name: web-smart-consumer
version: 1.2.0
description: "Mirror-less mode for a smart consumer connected over the web (the claude.ai web app, or any custom MCP connector) via the webagentoauth OAuth flow — no local filesystem, no per-turn bridge. You own ONE dedicated smart wiki (bound at consent, markerless, wiki-level ACL). Operate stateless per session: smart_bootstrap → wiki_admin_pull into context → edit → wiki_admin_push of only the pages you touched (never re-emit the whole wiki). No always-on recall: search/recall and save only on demand. Distinct from `smart-consumer`, which assumes a local .mwe/ working copy this transport does not have."
depends_on: ["core"]
applies_to:
  consumer_class: smart
  transport: web
status: implemented
---

# mwe-mcp / web-smart-consumer skill

This skill is for a **smart consumer reached over the web** — the claude.ai
web app, or any spec-compliant MCP custom connector — that authenticated through
the `webagentoauth` OAuth flow. You have your own subscription LLM (you are a
*smart* consumer), but unlike Claude Code you have **no local filesystem** and
**no per-turn host bridge**. That changes how you manage memory. If you are
working inside a project directory with a `.mwe/state.json`, use the
`smart-consumer` skill instead — this one is the mirror-less variant.

## Your dedicated wiki

At consent the user bound this connection to **one dedicated smart wiki** that
you own (its id looks like `<user>-<connection>`, e.g. `franz-claude`). It is a
**smart wiki**: markerless, content-indexed, with a single **wiki-level ACL** in
`_meta` (owner + `shared_with`). There are **no per-fragment `{{…}}` markers and
no per-fragment ACL** — you write plain markdown freely (create / edit / move /
rename / delete pages), exactly the way an engineering wiki is maintained. You
administer it with the `wiki_admin_*` family; you do **not** go through the
server-side ingest LLM for it.

## Stateless, mirror-less session loop

You keep **no local copy** of the wiki between sessions. Treat the server as the
single source of truth and work in-context:

1. **Start** — call `smart_bootstrap` to list the wiki(s) you own and any
   pending `_briefing.md` items.
2. **Load** — call `wiki_admin_pull` to bring the current pages into your
   context **before** you edit. Never edit from memory of a past session.
3. **Edit** — make changes in context.
4. **Write back** — call `wiki_admin_push mode=upsert` with **only the pages you
   actually changed**. Do **not** re-emit the whole wiki: a push carries exactly
   the pages you send, so re-sending unchanged (or half-remembered) pages would
   clobber good content. One page changed → push one page.

Because there is no local mirror and no optimistic-concurrency check yet, if the
wiki may have changed since your last `pull` (the user could have edited it from
the dashboard), **pull again before a batch of writes**.

## No always-on recall — search and save on demand

There is no per-turn recall block injected for you. So:

- **Recall / search only when it helps, or when the user asks** ("search in
  MwE", "what do you remember about X"). Do not assume context you did not
  explicitly fetch this turn. **Pick the right tool — they differ in scope:**
  - `recall_core_global` searches **only the user's OWN memory** (their personal
    wikis), and excludes project wikis. Use it for facts *about the user
    themselves* ("what's my doctor's name", "my preferences").
  - `wiki_search` searches **the whole corpus the user is allowed to read**,
    ACL-filtered — including *other people's / other entities'* pages they have
    access to. Use it for anything about someone or something **other than the
    user** ("when was Morgana born", "what's the office address"). A query about
    another person will return **nothing** from `recall_core_global` (it is
    owner-scoped by design), so go straight to `wiki_search` for those.
  - A `wiki_search` hit points you at a page; if the snippet doesn't carry the
    exact fact, `wiki_read` the page (pass its `path`) — the prose holds detail
    the snippet may omit.
  - `wiki_navigate` is the **deep** counterpart of `wiki_search`: a navigator
    walks the wiki structure hop by hop and returns the path it took as context,
    **plus** the flat hits (so it is a superset). It costs an LLM call per hop,
    so reach for it on a **question that needs depth or to connect things**
    ("tell me everything about X", "how does Y relate to Z"); use plain
    `wiki_search` for a quick one-line lookup. Steer it by passing `topics` and
    `owners` (e.g. `["user:morgana"]`) you already know from the conversation.
- **Save when asked** ("save this", "remember this", "save this chat"):
  - durable project knowledge → `wiki_admin_push` into your dedicated wiki;
  - a whole conversation / transcript → `wiki_ingest_external` (source `inline`,
    `format: dialogue`) and let the server digest it;
  - a personal fact about the user (cross-project, not project-bound) →
    `wiki_ingest_message`, which the server files into the user's personal
    memory.

## Briefing inbox

If `smart_bootstrap` surfaces pending `_briefing.md` items, read them at the
start of the session, act on them, and archive them (move into
`_briefing.archive.md`) as part of your next push.
