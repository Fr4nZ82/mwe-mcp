---
name: smart-codebase
version: 1.5.0
description: "Conversion + maintenance pattern for software-project companion-wikis: pre-existing CLAUDE.md/AGENTS.md documentation-rules scan (two options, logged in .mwe/state.json), build-an-mwe-wiki-from-docs / ingest-an-existing-wiki-faithfully (the local copy is never touched), modules/decisions/runbooks/architecture layout, deviation warnings, source_ref discipline, last_synced cadence."
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
`wiki-companion` type maps to a real codebase, how to convert
a pre-existing `docs/` directory into a companion-wiki, and the
day-to-day discipline (`source_ref` frontmatter, `last_synced` bumps)
that keeps REM's read-side sub-jobs useful.

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
Deviations don't error — they **warn** in the `wiki_admin_push`
response — but the standard layout lets REM's read-side dedup and your
team's recall hit the right files without surprise:

| Folder | What lives there | Page frontmatter convention |
|---|---|---|
| `modules/` | One page per source module / package (e.g. `modules/auth.md` for `src/auth/`). Behaviour, public interface, gotchas, why-it-is-the-way-it-is. | `module: <relpath>`, `source_ref: <relpath/glob>`, `last_synced: <ISO>` |
| `decisions/` | One page per non-trivial design decision. ADR-shaped (Context → Decision → Consequences). One file per decision; **do not** merge multiple decisions into a single file. | `decision_id: <slug>`, `status: proposed/accepted/superseded`, `superseded_by: <decision_id>?`, `last_synced` |
| `runbooks/` | One page per operational procedure (deploy steps, rollback, oncall response, recovery from $known-incident). Steps explicit, copy-pasteable. | `runbook_id: <slug>`, `severity: routine/oncall/incident`, `last_synced` |
| `architecture/` | Cross-module concerns: data flow, service topology, event/queue ownership. Diagrams (mermaid / ascii) go here. Fewer files than `modules/`, broader scope. | `topic: <slug>`, `last_synced` |
| `_briefing.md` (root) | Inbox managed by mwe-mcp — see `smart-consumer`. Never write directly via `wiki_admin_push`; use `wiki_admin_notify`. | (managed by the server) |
| `_meta.md` (root) | Auto-managed metadata: `owner_user`, `shared_with`, `wiki_type`. Edited via dashboard `/wikis/<id>/sharing`, not by the smart consumer. | (managed by the server) |

Other folders (`adr/`, `notes/`, `playbooks/`, `glossary/`) are
tolerated — `wiki_admin_push` returns them in the `warnings` list of
the response, the user sees the warning, but the push goes through.
There is a single bundled companion type and no custom-type
registration: a non-canonical layout simply keeps surfacing those
benign warnings. If the noise matters, conform to the canonical
folders; otherwise the warnings are harmless and the push still lands.

## Pre-existing `CLAUDE.md` / `AGENTS.md` documentation rules — surface, never silently obey

Many repos already carry a `CLAUDE.md` (or `AGENTS.md`) with rules about *how
to write documentation* — heading conventions, a required per-module structure,
a house style. **Before you generate the companion-wiki, scan `CLAUDE.md` and
`AGENTS.md` for documentation-style rules** and resolve the conflict with the user.
The companion-wiki has its own conventions (the four folders above,
`source_ref` / `last_synced`, markerless markdown); silently *following*
a repo's bespoke doc rules and silently *ignoring* them are both wrong —
each surprises the user.

Scope of the scan, for this Claude Code bridge: **`CLAUDE.md` and `AGENTS.md`**
— the two project-instruction files Claude Code reads natively. (`.cursorrules`
and the like ride a future Cursor bridge; do not read those here.) What counts
as a documentation-style rule:

- A heading like `## Documentation rules`, `## Wiki style`, `## Doc
  format`, `## Style guide`, `## Docs`.
- Imperative doc conventions in prose: "Every module must have a doc
  page", "Documentation conventions: …", "one ADR per decision", a
  mandated frontmatter shape, a required folder layout for docs.

If you find such rules, **show the user the exact lines** and offer
**two** options — there is no third:

- **(a) Adopt the mwe standard.** Ignore the repo's local doc rules for
  the companion-wiki; they stay on disk untouched, simply not applied.
  The wiki follows the bundled companion conventions above.
- **(b) Stop.** The user edits, removes, or adapts the doc rules in
  `CLAUDE.md` themselves, then re-runs the bootstrap. You do **not** edit
  `CLAUDE.md` for them.

There is no option to register a custom wiki type that formalises the
repo's rules: the single bundled companion type is markerless and
content-indexed, with no custom-type registration — a non-canonical
layout only ever *warns* on push.

**Record the choice in `.mwe/state.json`** so a later bootstrap does not
re-ask:

```json
{
  "bootstrap_decisions": {
    "claude_md_doc_rules": {
      "choice": "adopt_mwe",
      "scanned_headings": ["## Documentation rules"],
      "at": "2026-06-24T10:00:00Z"
    }
  }
}
```

`choice` is `"adopt_mwe"` (option a) or `"user_will_edit"` (option b,
pending the re-run). Never silently obey the repo rules, never silently
delete them, and never proceed past option (b) until the user has
re-run the bootstrap.

## Ingesting pre-existing docs or a wiki — never silently, the copy stays intact

You **never scan folders on your own initiative** to ingest them. This runs only
when the **user asks** to bring a project's documentation into mwe, or when a
write-moment forces the question (see `smart-consumer` → "the first write-moment
is the deferred bootstrap"). Whatever the shape, the **originals stay exactly as
they are** — you never rename, move, or delete them. Keep the user aware of every
step.

Where the **local mirror** (`state.local_wiki_root` — the directory you edit and
push from) ends up depends on the shape, and you record it in `.mwe/state.json`
at bootstrap:

- **Loose `docs/` → build from it:** you author a *fresh* wiki at the default
  `.mwe/wiki/`, reading `docs/` only as source. `local_wiki_root` stays
  `.mwe/wiki/`; `docs/` is untouched.
- **An existing wiki → ingest in place:** the existing directory (e.g. the repo's
  `wiki/`) **is** the mirror. Set `local_wiki_root` to it and keep editing it
  there — **never duplicate it into `.mwe/wiki/`**. `.mwe/wiki/` is only the
  default for a freshly-seeded wiki, not a mandatory location.

Three shapes, decided per project — and if it is ambiguous, **ask**:

### Loose `docs/`, no wiki → build an mwe-style wiki from it

Read the docs as source material, map them onto the companion layout (the four
folders below), and push. `docs/` is left untouched on disk (the user can diff
the generated `.mwe/wiki/` against it).

```
fn build_wiki_from_docs(cwd, wiki_type):
    confirm_with_user(
        "I'll read docs/ and author an mwe-style wiki in .mwe/wiki/ from it, then "
        "push it to mwe. Your docs/ stays exactly as it is. OK?")
    pages = []
    for f in walk("docs", "*.md"):                 # read-only — never renamed/removed
        cat = classify(f)                          # README/-arch → architecture/, ADR-* → decisions/,
                                                   # runbooks/ops → runbooks/, module docs → modules/
        pages.append({ "path": cat + "/" + slugify(f.stem) + ".md",
                       "content": rewrite_body(f, target_type=wiki_type) })  # +source_ref +last_synced
    if uncategorisable: surface_to_user("Couldn't classify <list>; parked in modules/.")
    out = wiki_admin_push(project_id=state.project_id, wiki_type=wiki_type,
                          smart=true, mode="create", pages=pages)
    write(".mwe/state.json", { wiki_id: out.wiki_id, last_op_log_head: out.op_log_id,
                               project_id: state.project_id,
                               local_wiki_root: ".mwe/wiki/",   # freshly authored → the default
                               source: "docs/" })
    return out.wiki_id
```

### An existing wiki (a markdown tree) → check compatibility, then ingest faithfully

Do **not** reclassify it. A smart wiki is markerless, so a good structure
survives a verbatim push. **Check it against the mwe rules first**:

- pages are markdown with **headings** (each section becomes a recallable unit);
- you do not hand-write `_meta.md` / `_captures` (the server owns them — a
  malformed `_meta` is rejected on push);
- no per-fragment `{{…}}` markers (smart wikis do not use them).

If it conforms, push it **page-for-page as-is** (`mode=create`, then `upsert`).
Where it needs light conformance (a missing heading, a stray `_meta`), adjust it
**with the user's awareness** and push. Never force the four-folder layout onto a
wiki already organised its own way — non-canonical folders only *warn* on push.

**The ingested directory stays the local mirror — do not copy it into
`.mwe/wiki/`.** Record its path as `local_wiki_root` in `.mwe/state.json` and go
on editing it in place; from here the day-to-day loop (`smart-consumer` →
"Day-to-day editing loop") reads and writes under that `local_wiki_root`.

```
fn ingest_existing_wiki(cwd, wiki_root):          # e.g. wiki_root = "wiki/"
    confirm_with_user(
        "I'll copy your existing " + wiki_root + " into mwe as-is (byte-for-byte) "
        "and keep editing it right there — I won't copy it into .mwe/wiki/. OK?")
    # Bulk copy runs in a SCRIPT — it reads the files and pushes them over /mcp;
    # you never pull the pages through your own context (no token burn, no read
    # ceiling). See smart-consumer "Onboarding an existing wiki — copy it up in bulk".
    wiki_id, head = run_bulk_copy_script(wiki_root,            # create → upsert in batches
                                         project_id=state.project_id, wiki_type=wiki_type)
    write(".mwe/state.json", { wiki_id: wiki_id, last_op_log_head: head,
                               project_id: state.project_id,
                               local_wiki_root: wiki_root,     # <-- the existing dir, NOT .mwe/wiki/
                               source: wiki_root })
    return wiki_id
```

**Oversized pages during ingest.** Because the bulk copy runs in a *script*,
not through your context, a page's size is a non-issue — the script reads a
350 KB `log.md` as easily as a 2 KB one, byte-for-byte, with no read ceiling
and no reassembly drift. Two things still hold:

- **Copy verbatim; never paraphrase.** The script emits the file bytes as the
  page `content` — a faithful copy, not a rewrite of it from memory.
- **A chronological `log.md` / `CHANGELOG.md` is copied whole, then curated.**
  It is first-class content — the trail maintainers read to retrace work — so
  you never drop it. *After* the copy, structure it by date and rotate it into
  dated period archives behind a live index (see "Change-log page"), as a
  follow-up pass — not something to skip or split during the copy itself.

### Both `docs/` and a wiki → decide with the user

No fixed rule. Usually the existing wiki is the base (check + ingest) and `docs/`
is a source to fold in — but surface both and let the user choose. Do not guess
silently.

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
- ❌ **Fighting the canonical folders.** Project one-offs are fine in
  the bundled type's `warnings` (you see the warning, the user sees it,
  life goes on). There is no custom-type escape hatch: if a layout
  diverges, either conform to the canonical folders or accept the benign
  warnings — don't invent machinery that no longer exists.

## Tools used

Same as `smart-consumer`, plus heuristics specific to source-tree
classification (README.md / docs/ / *.md detection in module roots,
ADR pattern recognition, runbook pattern recognition). The
heuristics live entirely on the consumer side — there is no MCP tool
for "classify this file".

## Cross-references

- Sibling skill: `smart-consumer` (parent, the cwd-bound mode this
  one specialises).
- Bundled `wiki-companion` type (the `wiki_type` stem + smart-consumer
  detection): `crates/mwe-core/src/smart.rs`.
- Engineering wiki: the smart-wikis design note.
- `_meta` / frontmatter constraints: the smart-wikis design note.
