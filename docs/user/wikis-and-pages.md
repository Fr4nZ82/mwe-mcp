# Wikis and pages

**Nav: Wikis.** `/dashboard/wiki`

The memory is also readable as ordinary pages of text. The list shows every
wiki you can read, with its **Wiki id**, **Title**, **Type** and the number of
**Active facts** it holds.

There are two families, on two tabs.

**Standard** — the memory the engine writes: one wiki per person, one per
group, and topic wikis that grow around a subject and belong to nobody. This is
where your own facts live. The engine owns the prose here: you correct the
facts, and the pages are rewritten from them.

**Smart** — a wiki a coding assistant keeps for a project. Its consumer writes
it whole and the engine only indexes what it is given.

At the top of the list, the same warning as everywhere: **prose pages lag the
conversation**. A fact you just said is stored and recallable immediately, but
its prose is written by the next scheduled run, together with the rest of that
page. Lists and notes you asked to keep are written straight away.

## One wiki

Opening a wiki shows its id, title, kind, short name and how many facts still
hold, then the list of its **Pages** with their sizes. Two page names recur:
`@profile.md`, the identity card, and `@rules.md`, the standing rules.

## One page

Opening a page shows it rendered, **as you are allowed to see it**. Where a
region was written for somebody else, it is replaced by `[redacted]` and a line
above the page says how many regions that was. You are never told what they
said.

At the bottom, the page states what it is and is not: *"To change this page:
leave inline comments, talk to the chat, or change the facts themselves on the
Facts page — who may read one, and when it holds. Nothing here rewrites the
text of the page directly."*

Two things you can do from here:

- **Add comments** — a switch that puts a **+ Comment** link beside every
  heading, so you can leave a note on one, and a **+ Comment on this page**
  link above the text for a note about the page as a whole. See
  [Comments](comments.md).
- **Edit page description** — the one line that says what this page is for. It
  steers where a new fact lands, and it is read when deciding whether to open
  the page at all. It appears on the pages of a wiki that is yours or your
  group's. The engine's page writer — the dashboard calls it the Cronista —
  composes a fresh description every time it rewrites the page, and that one
  replaces whatever you typed: what you write holds until the next rewrite and
  no longer. It is still worth typing when the memory is filing things badly
  right now.

The small `§` that follows a passage is not part of either: it opens the record
of the fact that passage was written from — its text, who said it, when it
holds and who may read it. That page is described in
[Your facts](your-facts.md).
