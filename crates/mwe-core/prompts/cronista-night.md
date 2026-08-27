---
name: cronista-night
version: 1.1
description: The Cronista's nightly half — the links a page already carries are what it said last time, offered for re-judgement rather than imposed. Spliced into the turn by `compiler::compile_leaf_page` ahead of the page itself, and ONLY on the full compile.
default_version_at_bootstrap: v1.0
part_of: cronista
appended_when: the compile is the nightly full pass, and the page already carries links of its own (it opens the task half, before the page)
---

# `cronista-night` — the night may change its mind about a link

A **part** of the `cronista` prompt (`crate::prompts::PromptOutput::PartOfAnother`),
spliced into the **task half** by `crate::compiler::compile_leaf_page` — right
after the `=== PAGE TO WRITE ===` marker, ahead of the page itself, because an
instruction about how to write a page comes before the page. Loaded via
`mwe_core::prompts::render("cronista-night", workdir, BUNDLED_CRONISTA_NIGHT_MD, vars)`.

## Runtime contract

- **When it is spliced in**: `dream::Cadence::Full` — the nightly REM compile or
  an operator-driven one — **and** the page carries links of its own. A page
  with none is asked nothing and pays nothing.
- **Model**: whatever the full cadence resolved, which is the `cronista` slot —
  the strong tier. That is the whole premise: this brief asks for a judgement
  the hourly model is not asked to make.
- **Placeholders**: `{prior_links}` — the `[[wikilinks]]` this page's own prose
  already carries, rendered by `compiler::plan_page_wikilink` like every other
  link feed. It **excludes** the rails the REM parked earlier tonight: those
  ride `RECOMMENDED LINKS` in the whole, where they are mandatory.
- **No `{locale}`**: the whole it rides opens with the language directive, and
  a second copy in one request is the operator paying twice for one instruction.

## Why the split exists

A link becomes permanent by being written: the plan takes each page's links
from its own prose (`planner::build_compilation_plan`, step 9), so whatever
reached the page text last time is handed back as a rail at the next rewrite.
Without this part that holds at every cadence, and the consequence is upside
down — the hourly pass runs on the cheap tier, so **the small model's sketch
binds the strong model's rewrite**, for as long as the page exists.

The engine already draws this line for **placements**: a parked move belongs to
the strong pass and an hourly one may not undo it. This is the same line, drawn
for links. The hourly pass writes what the page has and adds to it; the night
is the only pass that may take one away.

What the night may **not** drop is a rail the REM parked earlier the same
night: `rem::run_rail_writer` runs before the compile, so a compile free to
discard its decision would undo it in the minute it was taken. Those stay in
`RECOMMENDED LINKS` and stay mandatory. It is the night **after** — meeting the
link as ordinary prose — that may re-judge it.

## Prompt

```text
THIS COMPILE IS THE NIGHT PASS. You run once, on the strong model, over a page an hourly pass has already written. So you are asked one thing the hourly pass is not: to judge the links this page already carries, instead of inheriting them.

LINKS THIS PAGE CARRIED LAST TIME: {prior_links}

- These are the [[wikilinks]] the page's own prose already says. They are NOT an obligation and nothing checks that you wrote them. They are here so you know what was decided before you and do not rebuild the page's neighbourhood from nothing.
- Judge them by WHICH LINKS TO WRITE above, one fact at a time. For each fact you are writing, ask whether one of these is the page a reader of THAT fact needs next. When it is, keep it — and put it beside that fact, not wherever it happened to sit before.
- When a better destination exists among OTHER PAGES, write that one INSTEAD. Look harder for it than a hurried pass would: this brief exists because you are the pass that can afford to.
- When one extends nothing on this page, LET IT GO. Say nothing about it, write no farewell, leave no trace of it in the prose. A link that survives only because somebody once wrote it is how a memory silts up, and dropping it is a decision — yours, tonight.
- RECOMMENDED LINKS, listed with the page below, is a different list and it still binds. Those rails were decided earlier tonight, before you were called; not one of them is on the list above, and they are not yours to drop.
- Copy every link you keep CHARACTER-FOR-CHARACTER from the list above, exactly as WIKILINK GRAMMAR requires. A link retyped in the surrounding slug style points nowhere.
```
