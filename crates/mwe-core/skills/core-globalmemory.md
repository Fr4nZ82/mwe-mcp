---
name: core-globalmemory
version: 1.6.0
description: "Transversal mwe-mcp mode for smart consumers when the cwd has no .mwe/state.json: auto-recall across the user's wikis (forked subagent to prevent context bleed) AND manage your own dedicated operational wiki if you own one (general working memory + behaviour rules + conversations.md via wiki_admin_*). Facts about the user go to wiki_ingest_message; never dump everything into standard memory."
depends_on: ["core"]
applies_to:
  consumer_class: smart
  cwd_state: absent
status: implemented
---

# mwe-mcp / core-globalmemory skill

This skill governs the **transversal mode** of a smart consumer: the
cwd does not belong to a companion-wiki, but mwe-mcp is connected and
the user expects the agent to use it as a memory layer that survives
across sessions, projects, and machines.

Companion-bound work is a separate contract — see [`smart-consumer`](smart-consumer.md).
The bootstrap dispatcher in [`core`](core.md) decides which of the two
applies on each session start; this document only covers what to do
when the dispatcher lands here.

## When this skill applies

All three conditions must hold:

- Your `consumer_class` claim is `smart`.
- Your current working directory does **not** contain `.mwe/state.json`
  (so this is not a companion-bound session — see `smart-consumer`
  for that).
- You are connected to a mwe-mcp server (the connection is active,
  not overridden to `null` for this workspace, and the JWT is valid).

If the `null` override is in effect for this workspace (NDA project,
endpoint switch), neither this skill nor `smart-consumer` runs: the
consumer works isolated and does not call any `wiki_*` tool against
the personal mwe-mcp.

## Your operational wiki (if you own one)

If you signed in over OAuth, a dedicated **operational wiki** was forged for you
— `smart_bootstrap` surfaces it among the wikis you own and marks it
**`is_self: true`** (its slug equals your `consumer_id`), so you don't have to
guess which is yours when the user owns several agent wikis. It is **your home**,
owned by your user: keep your general working
memory, your **behaviour rules**, and a dated **`conversations.md`** log there.
You author it with `wiki_admin_*` (read with `wiki_admin_pull`, write touched
pages with `wiki_admin_push mode=upsert`) — like a project wiki, but it is not
project-bound and it exists in **every** session, not only inside a repo.

This is distinct from the user's **standard** memory: facts *about the user*
(preferences, appointments, who people are) still go to `wiki_ingest_message` so
they stay canonical and other consumers see them. Your operational/general notes,
your rules, and the conversation log go to **your** wiki. Capture as you go — do
not wait to be asked.

## Cardinal pattern — transversal recall on the first user prompt

On the **first user prompt of a new conversation** in a cwd that
matches the conditions above, issue one transversal recall call
**before composing your reply**. Use the dedicated K-family tool:

```jsonc
recall_core_global({
  query: "<the user prompt, lightly cleaned of greetings>",
  // optional: limit defaults to 8, server clamps to [1, 20]
})
```

The server applies the canonical filter for you — caller-owned wikis
(`owner_user = user:<your sender_id>`), companion family excluded.
The response echoes the filter in `filter_applied.excluded_wiki_types`
so the audit trail is unambiguous.

Why a dedicated tool rather than `wiki_search` with a hand-rolled
filter:

- `recall_core_global` bundles the canonical filter, so you do not
  hand-roll it. Call it yourself on the first prompt and on a topic
  shift; the Claude Code session-start nudge reminds you, but the call
  is **yours** — recall is model-driven under OAuth (there is no token
  for a shell hook to call `/mcp` with).
- `recall_core_global` is gated on `consumer_class=smart` — a
  standard/conversational token can't accidentally trigger it.
- The filter is centralised: when a new "standard" family is added
  (or when we tighten the companion exclusion semantics), the
  filter changes in one place, not in every consumer's prompt.

If you do not have the K-family tool yet (older deployment) or
the hook is disabled, fall back to the explicit shape:

```jsonc
wiki_search({
  filter: { owner_user: "<your sender_id>" },
  query: "<the user prompt, lightly cleaned of greetings>"
})
```

— then drop hits whose `wiki_type` belongs to `family=companion`
(see `wiki_type_describe` if you don't already know the set). The
`recall_core_global` tool is exactly this fallback, server-side, with
the gates pre-wired.

If the recall returns hits, weave them into your reply with a citation
(`wiki://<wiki_id>/<path>` — see companion-wikis.md §citation-ids
for the format; standard wikis use the same scheme). If it returns
nothing, answer from the prompt alone and move on — do not invent
context that mwe-mcp did not return.

After the first prompt, do **not** spam `wiki_search` on every turn.
Recall again only when the user introduces a new topic that the
recent conversation has not covered, or when you genuinely need to
check whether a fact is already recorded before contradicting it.

## Asking about someone/something else — use `wiki_search`, not `recall_core_global`

`recall_core_global` is **owner-scoped to the user** by design (caller-owned
wikis only), so a question about *another person or entity* the user knows — a
contact's birthday, a colleague's role, a shared address — returns **nothing**
from it. For those, call `wiki_search` with **no `owner_user` filter**: it spans
the whole corpus the user is allowed to read (ACL-filtered), including other
people's pages they have access to. If the top snippet doesn't carry the exact
fact, `wiki_read` the page it points to — the prose holds detail the snippet may
omit. (Empirically confirmed: a "when was X born" query draws a blank on
`recall_core_global` but lands the right page via `wiki_search`.)

For a question that needs **depth or to connect things across pages** ("tell me
everything about X", "how does Y relate to Z"), use **`wiki_navigate`** instead
of `wiki_search`: a navigator walks the wiki structure hop by hop and returns the
path it took as context, plus the flat hits (a superset of `wiki_search`). It
costs an LLM call per hop, so keep `wiki_search` for quick one-line lookups.
Steer it with `topics` and `owners` (e.g. `["user:<id>"]`) you already know.

## Forked-subagent recall — run the search in an isolated context

The recall above is a **contract requirement**, not an option: run it
in a **forked subagent**, not in the main conversation thread. The
main thread receives only the distillate. The reason is context
hygiene — `wiki_search` returns excerpts, `wiki_read` returns whole
pages, and a verbose recall pollutes the main agent's working context
with material the user did not ask for, pushing useful prior turns
out of the cache window and biasing subsequent reasoning toward
whatever the search happened to surface.

Concretely:

- **Claude Code consumer**: spawn an `Agent` with `subagent_type:
  Explore` (read-only search) or `general-purpose` when the recall
  needs to combine `wiki_search` + `wiki_read` + light synthesis.
  Hand the subagent a self-contained prompt that includes the user
  query and the scope filter (`owner_ids`, `wiki_types` allowlist),
  ask for a short report (target ≤ 200 words, citing
  `wiki://<wiki_id>/<path>` for each fact). Use the distillate
  verbatim in your reply; do **not** re-issue `wiki_search` in the
  main thread.
- **Codex / Cursor / Cowork / other consumers**: use the equivalent
  subagent / sub-task mechanism your framework provides. Most
  modern agent frameworks ship one — if yours does not, fall back
  to a single `wiki_search` call with a tight result limit (5-8
  hits) and immediately summarise the hits in your own words before
  composing the user-facing reply, so the raw excerpts do not
  linger.

Self-contained prompt the subagent gets (Claude Code shape; adapt
for other frameworks):

```text
Search the user's mwe-mcp standard wikis for context relevant to
this query:

  "<user query, lightly cleaned of greetings>"

Use:
  wiki_search({
    query: "<user query>",
    scope: {
      owner_ids: ["<your sender_id>"],
      wiki_types: ["wiki-user","wiki-tech","wiki-lists"]
    }
  })

If a hit is promising, read it with wiki_read. Return ≤ 200 words:
bullet list of relevant facts, each followed by the
wiki://<wiki_id>/<path> citation. If nothing is relevant, return
exactly "no relevant memory."
```

The output of that subagent is the only thing the main thread sees.
The raw excerpts stay in the subagent's window, scoped to the recall
task, and disappear when the subagent returns.

Edge cases:

- If the subagent reports "no relevant memory", answer the user from
  the prompt alone — do not try a broader recall in the main thread
  hoping to find something. A broader recall is also a forked
  subagent call.
- If the user's question is a **direct follow-up** to a prior
  recall earlier in the same conversation, skip the subagent and
  reuse the prior distillate. The forked recall is a per-topic
  cost, not a per-turn cost.
- The forked subagent **must not write** to mwe-mcp. Recall is
  read-only. Writes (when needed — see "Auto-memory integration"
  below) happen in the main thread so the user sees them.

This pattern is a **firm part of the skill contract**, not a
suggestion: a transversal recall that runs in the main thread breaks
the budget assumption that makes mwe-mcp affordable as a
persistent-memory layer for smart consumers.

## Auto-memory integration (consumer-agnostic)

Every smart consumer ships its own auto-memory mechanism — Claude
Code keeps per-project notes in `~/.claude/projects/<hash>/memory/`,
Codex has its own schema, Cowork another. **mwe-mcp does not replace
these auto-memories**. The convention is:

| Layer | Scope | Lifetime | Use for |
|---|---|---|---|
| Consumer auto-memory (local file) | per-project, per-machine | until consumer cleanup | session state, ephemeral operational details, in-flight TODO scratchpad |
| mwe-mcp standard wikis | per-user, cross-project, cross-machine | deployment lifetime | stable facts, decisions, preferences, references that the user will want again from a different cwd or a different consumer |

Concretely, when you observe something worth remembering:

1. **Is it tied to this exact cwd / this session?** (e.g. "the script I
   am editing right now expects `--format=json`"). → Save it to your
   auto-memory. mwe-mcp would clutter for nothing.
2. **Is it a fact about the user, a cross-project decision, a stable
   preference, or a reference the user will want from elsewhere?**
   (e.g. "user prefers `pnpm` over `npm` on every Node project") →
   Save it to mwe-mcp **and** echo a one-liner pointer to your
   auto-memory so the next session in this cwd does not have to
   round-trip to discover it.

The skill is neutral about *how* your consumer writes to its local
auto-memory — that mechanism is yours. What it prescribes is *what*
goes where.

For mwe-mcp writes from transversal mode, route by target (see `core`):
a fact **about the user** → `wiki_ingest_message` against the user's
`wiki-user` (the server's internal LLM handles routing + capture); your
own **operational** notes / behaviour rules / conversation log → your
**operational wiki** via `wiki_admin_push`. Only *project* companion-wikis
are off-limits here (no project cwd) — your own operational wiki is not.

## Cwd discrimination — recognizing a companion candidate

Even though the dispatcher landed on this skill (no `.mwe/state.json`
in cwd), the cwd may still be a candidate for **promotion** to a
companion-bound session — for example, the user opened a real project
folder for the first time and never bootstrapped it.

Heuristics that the cwd is a companion candidate (not a generic
folder):

- Contains a VCS marker (`.git/`, `.hg/`, `.svn/`).
- Contains a project manifest (`Cargo.toml`, `package.json`,
  `pyproject.toml`, `pom.xml`, `Gemfile`, `go.mod`, …).
- Contains a `docs/` or `wiki/` subtree that looks like
  human-maintained project documentation.
- Contains a `CLAUDE.md` / `AGENTS.md` / `.cursorrules` /
  `.continuerules` (consumer instruction file that implies a
  project-shape).

If two or more of these match — **or** at any point mid-session you
judge a moment to be durable *project* knowledge while this cwd is not
bootstrapped — **derive the project's stable `project_id`** and look it
up before proposing anything:

```jsonc
// project_id = sha256( normalized_vcs_origin + ":" + cwd_relpath_from_repo_root )[..16]
// A CLAUDE.md line "mwe-mcp: project_id=manual:<slug>" overrides the derivation.
smart_bootstrap({ project_hint: "<project_id>" })   // works even with NO local .mwe/state.json
```

The lookup splits the proposal you make — and in **both** branches you
**propose, you do not act**:

- **A companion-wiki already exists for this `project_id`** (it was
  bootstrapped on another machine, or the local `.mwe/` was wiped) →
  stop and **propose a sync**: `wiki_admin_pull` the server state into a
  fresh `.mwe/wiki/`, write `.mwe/state.json`, then resume companion-bound
  work. **Never** start a second wiki for a project that already has one,
  and never write into it before the pull/reconcile.
- **No companion-wiki anywhere for this `project_id`** → **propose
  creating** a new project companion-wiki.

Either way **do not auto-bootstrap silently**. The user might be browsing
a read-only checkout, or want this folder to remain transversal-only.
Bootstrap / sync is a write operation (it renames an existing `docs/`,
generates pages, and pushes to the server — a smart wiki is markerless
and content-indexed, so there is **no** custom `wiki_type` to register)
and must be explicit.

If the user consents, switch to `smart-consumer`: load that skill via
`skill_fetch` (or your consumer's equivalent) and follow its
`smart_bootstrap` checklist (create-new vs. sync-existing, plus the
pre-existing-`CLAUDE.md` documentation-rules check). From that point on
you are in companion-bound mode for the rest of the session — this skill
becomes irrelevant for this cwd until `.mwe/state.json` is removed again.

If none of the heuristics match, treat the cwd as genuinely generic
(scratch folder, home dir, a temporary workdir) and stay in
transversal mode without prompting.

## Operational anti-patterns

- ❌ **Don't recall on every turn.** Once per topic shift, not once
  per prompt. The user is paying for your token budget; redundant
  recalls burn it without information gain.
- ❌ **Don't drop the `wiki_types` allowlist from `scope`.** Without the
  allowlist, the search will hit companion-wikis the user owns and
  leak project context into a generic session.
- ❌ **Don't write to a *project* companion-wiki from here.** Without a
  project cwd you have no `project_id` context and no `.mwe/state.json` to
  track op_log_head, so a project-wiki write would be guesswork. Your own
  **operational wiki** is the exception — it is not project-bound, so
  writing to it via `wiki_admin_push` here is correct.
- ❌ **Don't merge what the auto-memory holds and what mwe-mcp holds
  into a single layer.** They have different lifetimes; conflating
  them produces stale facts on one side and missing facts on the
  other.
- ❌ **Don't silently auto-bootstrap a candidate cwd.** Always ask.
  See `smart-consumer` for the conversion flow once the user
  consents.

## Cross-references

- Design narrative: [tool reference](../../../docs/protocol/tool-reference.md).
- Memory-model invariants: [memory model](../../../docs/concepts/memory-model.md).
- Reference scenarios A & B (transversal recall + promotion to
  companion-bound): ingest pipeline.
- Sibling skill: [`smart-consumer`](smart-consumer.md) — load this
  when the cwd promotes to companion-bound.
- Engineering wiki: [the companion-wikis design note](https://github.com/Fr4nZ82/mwe-mcp/blob/main/the companion-wikis design note).
