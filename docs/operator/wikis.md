# Wikis, from the admin's side

**Nav: Wikis.** `/dashboard/wiki`

Everybody sees this page — [Wikis and pages](../user/wikis-and-pages.md) is the
reader's half of it. This is what an admin can do there that a reader cannot.

## The raw page editor

On a **standard** wiki an admin can edit a page's Markdown directly. It is
admin-only, and it does not exist on a smart wiki at all: there the reader's
channel is a comment to the consumer that owns the wiki.

Reach for it rarely. The prose of a standard wiki is compiled out of the facts,
so a hand edit survives only until the next rewrite of that page. The durable
fix is the fact: [the Facts page](../user/your-facts.md), or a comment.

## Exporting a wiki

`/dashboard/wiki/<id>/export` downloads the wiki and everything under it as a
tar archive, with every fragment carrying its governance inline — subject,
audience and sender in the text itself — so the archive reads on its own. It is
admin-only, and it is refused outright when the reveal lens has been locked in
`mwe-mcp.config.yaml`, because the archive is unredacted by construction.

## Deleting a wiki

The **Actions** column carries **delete** — the most destructive control in the
panel. A living person's or group's own wiki shows `—` instead: that one is
removed through [Export and forget](export-and-forget.md), not from here.

The confirmation page counts the blast radius and asks you to type the wiki id.
Deleting moves the whole directory subtree into `<workdir>/trash/`, where it
waits — 30 days out of the box — and is then erased for good. Putting it back
before then is a file move.

What happens to the **facts** on it is a separate choice, and for a wiki that
holds any it is the one that matters:

- **Dissolve (recommended)** — the structure goes, the knowledge stays. Nothing
  is closed: every fact is freed and re-placed where it belongs across the rest
  of the memory, by the same pass that files new facts.
- **Return to each author** — each fact somebody else contributed is handed
  back intact and re-placed where its subject lives; the ones you contributed
  are closed, and so are facts with no home to return to.
- **Tombstone all** — close every fact, including the ones other people
  contributed. They leave recall at once and survive as records that something
  was there.

A closed fact leaves recall, and putting the directory back does **not** bring
it back. That asymmetry is the reason the choice is on the page.

## The Smart tab

Smart wikis are the ones a consumer writes and owns; the engine only indexes
what it is given, and the nightly cycle keeps its hands off their contents. The
listing adds **Last push** and **Unread briefing**, and three consoles per wiki.

**briefing** — the wiki's inbox. Notices for the consumer land here and it
reads them when it next starts up, archiving each one as it acts. The file is
created on demand by the first notice; until then the page says so. Anybody the
wiki admits as a reader can open it.

**op-log** — an append-only record of every write a consumer made to this wiki:
who, what, when. Readable by the same people, but the **Revert** button on a
push is the admin's, and the page states its two limits honestly: a revert is
refused when any later operation touched the same page, and it is offered only
while the page bodies that push overwrote are still kept — the undo window, 30
days out of the box (see [Backup](backup.md)). After that the row stays and the
button does not.

**sharing** — the roster of who else may read the wiki. It is **owner-only**:
the page edits `user:<id>` and `group:<id>` entries on a wiki that has an
owning person, and a wiki owned by a group refuses it with a message saying
why.
