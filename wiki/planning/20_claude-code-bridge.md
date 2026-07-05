---
title: Claude Code bridge — the OAuth smart-consumer onboarding surface
status: in-progress
---

# 20. Claude Code bridge

**Shipped (20a–20f), live on the deployed server.** Claude Code connects to a
running mwe-mcp as the user's own **Smart** consumer over the **`webagentoauth`
OAuth flow** — `claude mcp add --transport http mwe-mcp <origin>/mcp --scope user`
+ an OAuth sign-in, **no token to mint or paste**. The redirect picks the profile
(loopback → `Local` with the full tool catalog incl. `skill_fetch`; claude.ai's
https → `Web`). Recall is **model-driven** (the only hook is one optional
token-less `SessionStart` nudge); the dedicated wiki is an **operational wiki**
(general memory + behaviour rules + `conversations.md`) plus the per-project wikis
it authors; routing is **3-way** (operational / project wiki / the user's standard
memory), never dumping everything into standard; bootstrap is never automatic;
pre-existing docs/wiki ingestion is user-initiated and leaves the local copy
intact; the pre-existing-rules scan covers **`CLAUDE.md` + `AGENTS.md`**. Current
state lives in [web-agent-oauth.md](../design-notes/web-agent-oauth.md),
[agents-bridges.md](../development/agents-bridges.md),
[INTEGRATING.md](../../INTEGRATING.md), and the bundled skills
`smart-consumer.md` / `smart-codebase.md`.

## Remaining work

- [ ] 20g — **Live dogfood end-to-end + go-live confirmation.** Run Claude Code
  from a **separate machine** against **prod `https://mwe.contea.casa`** (the OAuth
  endpoints must be HTTPS, so the dance runs against prod, not a local `http`
  serve): OAuth connect → load the operational wiki → recall/capture → a project
  bootstrap exercising the corrected ingestion (local copy intact,
  `CLAUDE.md`/`AGENTS.md` scan). The served `/bridges/claude-code` page and the new
  behaviour are already live on the deployed binary.

## Caveats for the dogfood

- **Trust boundary** — Claude Code is the canonical shell+file-tool host: a workdir
  co-located with it is readable raw. Fine for single-principal personal use and
  the recommended remote-HTTP topology against `mwe.contea.casa`; never co-locate a
  multi-principal workdir with it
  ([INTEGRATING.md §"Deployment security"](../../INTEGRATING.md)).
- **Dead-design traps for the bridge instructions** — there is no `snapshot_replace`
  push mode (a full rewrite is `upsert` + a `delete` list); `since_op_log_id`
  delta-pull is deferred (always full-pull, path-narrow with `paths=`).
