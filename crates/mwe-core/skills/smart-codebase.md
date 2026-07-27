---
name: smart-codebase
version: 1.6.0
description: "Maintenance pattern for a software project's smart wiki: modules/decisions/runbooks/architecture layout, module- and decision-page conventions, the change-log page, source_ref discipline, last_synced cadence. First connect (importing an existing docs/ or wiki, the CLAUDE.md documentation-rules resolution, the shape report) is not here — it lives in smart-onboarding."
depends_on: ["core", "smart-consumer"]
applies_to:
  consumer_class: smart
  cwd_state: present
  project_kind: software
status: implemented
---

# mwe-mcp / smart-codebase skill

This skill concretises `smart-consumer` for the most common case:
**a software project's companion-wiki**. It defines how the bundled
`wiki-companion` type maps to a real codebase, what belongs on a module,
decision or change-log page, and the day-to-day discipline (`source_ref`
frontmatter, `last_synced` bumps) that keeps REM's read-side sub-jobs
useful. Bringing an existing `docs/` tree or wiki *in* is a one-shot job
and lives in [`smart-onboarding`](smart-onboarding.md).

## When this skill applies

Loaded when the dispatcher in `core` has already selected
`smart-consumer` **and** the project in cwd is a software project.
Heuristic for "software project" (any one of):

- A VCS marker (`.git/`, `.hg/`).
- A language-specific manifest (`Cargo.toml`, `package.json`,
  `pyproject.toml`, `go.mod`, `pom.xml`, `build.gradle`, `Makefile`,
  `CMakeLists.txt`, etc.).
- A `docs/` or `documentation/` directory with at least one `.md` /
  `.rst` file.

When none of these match, stay on `smart-consumer` alone — the
codebase-specific patterns below won't help.

## Folder-structure mapping

The bundled `wiki-companion` type suggests four top-level
subdirectories under your local mirror (`state.local_wiki_root` —
`.mwe/wiki/` by default, or the directory you ingested in place).
Deviations are neither an error nor a warning — the server does not
check folder shape at all — but the standard layout lets REM's
read-side dedup and your team's recall hit the right files without
surprise:

| Folder | What lives there | Page frontmatter convention |
|---|---|---|
| `modules/` | One page per source module / package (e.g. `modules/auth.md` for `src/auth/`). Behaviour, public interface, gotchas, why-it-is-the-way-it-is. | `module: <relpath>`, `source_ref: <relpath/glob>`, `last_synced: <ISO>` |
| `decisions/` | One page per non-trivial design decision. ADR-shaped (Context → Decision → Consequences). One file per decision; **do not** merge multiple decisions into a single file. | `decision_id: <slug>`, `status: proposed/accepted/superseded`, `superseded_by: <decision_id>?`, `last_synced` |
| `runbooks/` | One page per operational procedure (deploy steps, rollback, oncall response, recovery from $known-incident). Steps explicit, copy-pasteable. | `runbook_id: <slug>`, `severity: routine/oncall/incident`, `last_synced` |
| `architecture/` | Cross-module concerns: data flow, service topology, event/queue ownership. Diagrams (mermaid / ascii) go here. Fewer files than `modules/`, broader scope. | `topic: <slug>`, `last_synced` |
| `_briefing.md` (root) | Your inbox — see `smart-consumer`. Others reach it with `wiki_admin_notify`; **you** write it with an ordinary `wiki_admin_push` (the server refuses a smart consumer notifying its own wiki). | (you own it; the server appends to it) |
| `_meta.md` (root) | Auto-managed metadata: `owner_user`, `shared_with`, `wiki_type`. Edited via dashboard `/wikis/<id>/sharing`, not by the smart consumer. | (managed by the server) |

Other folders (`adr/`, `notes/`, `playbooks/`, `glossary/`) are simply
tolerated: there is a single bundled companion type and no custom-type
registration, and nothing on the server objects to a layout of your own.
Never force the four folders onto a wiki already organised its own way.

What the push response's `warnings[]` **does** carry is page **shape** —
a page whose blocks are too long for the index to keep whole. That is a
retrieval problem and worth acting on; folder names are not.

## Bringing an existing `docs/` or wiki in — not here

That is **first connect**, it happens once, and it lives in
[`smart-onboarding`](smart-onboarding.md): the faithful bulk copy (bytes go
file → script → server, never through your context), the pre-existing
`CLAUDE.md` / `AGENTS.md` documentation-rules resolution, the shape report,
and the repair proposal. Two things from it are worth remembering even when
you never open that skill:

- The originals are **never** renamed, moved, or deleted, and nothing is
  rewritten on the day it is imported.
- If the mirror is an existing directory (e.g. the repo's `wiki/`), that
  directory **is** `state.local_wiki_root` — it is never duplicated into
  `.mwe/wiki/`, which is only the default for a wiki you seed fresh.

## Module-page convention

One page per source module. The frontmatter ties the doc to its
source:

```yaml
---
module: src/auth
source_ref:
  - src/auth/**
  - tests/auth/**
topic: authentication
status: implemented
last_synced: 2026-05-25T18:00:00Z
---

# Auth module

(prose body — behaviour, public interface, gotchas, decisions
references, etc.)
```

`source_ref` is a glob (or list of globs) pointing at the code the
page documents. It does **two** things:

1. Lets REM's dedup-source sub-job detect when two pages claim the
   same `source_ref` (likely duplicate; opens a `dedup_proposed`
   notify in `_briefing.md`).
2. Lets the user check coverage from the dashboard `/wikis/<id>` view
   (planned): "which source modules have no companion
   page?".

**Do not** put per-function or per-line details in module pages. The
companion-wiki is for the why, not the what — the what lives in the
source itself, where it can't drift.

## `last_synced` discipline

Every `wiki_admin_push` that materially updates a page bumps
`last_synced` in the frontmatter to the current time. Cosmetic edits
(typo, link fix) **don't** bump — they would reset the freshness
signal REM uses for read-side preference.

REM's recall pre-indexing sub-job prefers pages with recent
`last_synced` when the user's query hits the same topic from
multiple pages. Pages with `last_synced` older than 90 days (no
edits) get a soft staleness warning in `_briefing.md` from the
Briefing dispatcher — the user can confirm "still accurate, bump
the date manually" or "rewrite this section".

A typical edit cycle:

```
# User: "the auth module's session refresh is now JWT-based, not cookie-based"
local_edit("modules/auth.md",
    section="Session refresh",
    body="... (rewritten paragraph) ...")
bump_last_synced(local_path="modules/auth.md")

wiki_admin_push(
    wiki_id=state.wiki_id,
    mode="upsert",
    pages=[{"path": "modules/auth.md", "content": read_local(...)}],
)
```

## Decision-page convention

ADR-shaped: one decision per file, slugified id, status field that
tracks supersession.

```yaml
---
decision_id: jwt-refresh-2026-q2
status: accepted
date: 2026-05-15
superseded_by: ~
last_synced: 2026-05-25T18:00:00Z
---

# Use JWT for session refresh instead of cookies

## Context
(why we're deciding now)

## Decision
(what we chose)

## Consequences
(what it implies, what becomes harder)

## Alternatives considered
(briefly — the ones we rejected and why)
```

When a decision is superseded, **do not delete the old page** — flip
its `status: superseded` + `superseded_by: <new-decision-id>` and add
the new decision as a separate page. The old page stays so
historical context survives; REM's recall surfaces both pages when
the user asks about the underlying topic and the
`superseded_by` chain lets the user follow the trail.

## Change-log page — first-class, structured by date, rotated (never dropped)

A project wiki often carries a root-level append-only changelog
(`log.md`, `CHANGELOG.md`, a `decisions/` index) — a chronological trail,
like this repo's own `wiki/logs.md`. It is **first-class content:**
maintainers read it to retrace what was done, in order — **never exclude
it, never atomise it into facts.** Do not dismiss it as "redundant with
the op-log": mwe's server-side op-log is a low-level audit of *page
writes*, while a curated changelog is a human narrative of *what changed
and why* — a different artefact, and the one people actually read.

It just needs the **append-only log-page discipline** from
`smart-consumer` ("keep them bounded, rotate by period"), plus two
log-specific habits, because a chronological log is the page most likely
to breach a single read:

- **Navigated by date, so structure it by date.** A log is recalled by
  *when*, not by topic. Give each period (or each entry) a dated heading,
  newest first, so every window is its own heading-delimited section: the
  maintainer can scan the trail and recall can land on the right window. A
  wall of undated bullets embeds as one slab and defeats both.
- **Rotate by period, keep a live index.** The live page holds the current
  period; older periods roll into dated archives (`log.<YYYY>.md`,
  `log.<YYYY-Qn>.md`) that the live page **links**, so the timeline stays
  navigable across the rotation. Push the trimmed live page and the new
  archive together (atomic). You bound each page's mass without breaking
  the trail — nothing is discarded.

**Ingesting an existing oversized log:** split it into dated period
archives at ingest — verbatim, entry-for-entry — behind a live index page
that links them, rather than pushing one 350 KB slab. This is where the
general "read oversized pages in offset-chunks, verbatim" rule and the log
rotation meet: the log survives whole, just paged and navigable.

## Anti-patterns

- ❌ **One giant `architecture/overview.md` for everything.** Split
  by concern (data-flow, topology, deployment, etc.). REM's recall
  preference can't help if every architectural query hits the same
  monolithic page.
- ❌ **Module pages that paraphrase the code.** That kind of doc
  decays fastest. Write the why and the contracts; trust the source
  for the what.
- ❌ **Embedding diagrams as binary attachments.** Use mermaid /
  asciidiagrams in the page body. The companion-wiki is markdown +
  Obsidian — binary attachments don't render in the dashboard and
  REM can't index them.
- ❌ **Bumping `last_synced` on cosmetic edits.** You burn the
  freshness signal REM uses for recall preference.
- ❌ **Fighting a layout that already works.** The canonical folders are
  a suggestion the server never checks; a project with its own structure
  keeps it. There is no custom-type escape hatch and none is needed —
  don't invent machinery that does not exist.

## Tools used

Same as `smart-consumer`, plus heuristics specific to source-tree
classification (README.md / docs/ / *.md detection in module roots,
ADR pattern recognition, runbook pattern recognition). The
heuristics live entirely on the consumer side — there is no MCP tool
for "classify this file".

## Cross-references

- Sibling skills: `smart-consumer` (parent, the cwd-bound mode this one
  specialises) and [`smart-onboarding`](smart-onboarding.md) (first
  connect: the import, the shape report, the repair proposal).
- Bundled `wiki-companion` type (the `wiki_type` stem + smart-consumer
  detection): `crates/mwe-core/src/smart.rs`.
- Engineering wiki: the smart-wikis design note.
- `_meta` / frontmatter constraints: the smart-wikis design note.
