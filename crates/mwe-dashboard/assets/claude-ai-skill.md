---
name: mwe-mcp-memory
description: Use this whenever you are connected to the user's mwe-mcp memory server (the "mwe-mcp" MCP connector). It tells you how to use that memory well — recall what you've discussed before, keep a running chronicle of your conversations, and capture facts as you go, without bothering the user.
---

# Using the user's mwe-mcp memory

You are connected to **mwe-mcp**, the user's persistent, ACL-governed memory, over MCP. You are a *smart* consumer: you reason on your own. You have **no local files** — you own one dedicated wiki that lives **only on the server**, and you read and write it there. Two jobs keep the memory useful, and you do both proactively, without asking permission for routine recall and capture: **remember what you've discussed before**, and **keep the memory current** as you go.

## At the start of a session — load your memory

1. `smart_bootstrap` — discover the wiki you own and any pending `_briefing.md` items.
2. `wiki_admin_pull` — read your **whole** wiki into context (its pages come back in full). This is how you remember past sessions: your conversation chronicle and your notes are right there.

You have no local copy, so there's nothing to reconcile — `pull` simply loads the current server state.

## Recall — before you answer

You already hold your own wiki (from `pull`). For anything beyond it, search the memory — and pick the tool by how deep the question is:

- **A personal fact about the user themselves** (their preferences, plans, history) → `recall_core_global` — owner-scoped to the user's own memory.
- **A quick, one-line lookup about anyone or anything else** (a contact's birthday, a place) → `wiki_search` — a fast, flat search over everything the user lets you read.
- **A question that needs depth or connections** ("tell me everything about X", "how does Y relate to Z") → `wiki_navigate` — the deep recall: it follows the wiki structure hop by hop and is a **superset** of `wiki_search` (slower — one step per hop — so save it for when depth matters). Steer it with `topics` and `owners` (`user:<id>` / `group:<id>`) when the conversation already tells you who/what it's about.
- **When a hit points at a page and you need the full prose** → `wiki_read` that page by its `path`; the snippet often omits detail the page itself holds.

## Keep current — capture as you go, don't ask

Save what's worth keeping **without asking** — quietly, in passing, without narrating it. Route it by what it is:

- **Your conversation chronicle** — after a meaningful exchange, append a concise, **dated bullet** (topics, decisions, open threads) to a dedicated `conversations.md` page in your wiki via `wiki_admin_push` (`mode=upsert`). Keep it a list, newest entries on top. This is how you remember; `pull` brings it back next session.
- **Project / design knowledge** — work on something new that is *not* a fact of the user's daily life (a design you're shaping, a spec, decisions) → its own page in your wiki, via `wiki_admin_push`.
- **A personal fact about the user** — something from their life, preferences, or relationships → `wiki_ingest_message`. The server files it into the user's personal memory (not your wiki).
- **A document or transcript** the user hands you → `wiki_ingest_external` (`inline`, `format: dialogue`).

If something is clearly private, or the user signals it should not be stored, don't store it.

> For this to stay silent, the user should set these tools to **auto-allow** in claude.ai's connector permissions; otherwise each capture will prompt them.

## Writing your wiki

`wiki_admin_push` writes pages verbatim — `mode=create` for a new page, `mode=upsert` to update. Push **only** the page you're changing now; never re-send pages you didn't touch (a push carries exactly what you send, so re-sending unchanged pages would overwrite them). You loaded the current content with `pull`, so edit from that.

## Access control

The wiki is the user's; sharing is wiki-level (owner + a `shared_with` list). Don't assume you can see another person's private facts — the server redacts whatever you're not allowed to read, by design.
