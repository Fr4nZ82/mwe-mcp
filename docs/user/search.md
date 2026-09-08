# Search

**Top bar: the box on the left.** `/dashboard/search`

Type a word and press enter. What comes back is the pages that carry it,
grouped by wiki, each with the line the word appears in and a link that opens
the page.

The box sits in the top bar on every page, so you can ask from wherever you
are. Opening `/dashboard/search` on its own gives you the same box, larger,
with a **Search** button.

## What counts as a match

**A word finds the words that begin with it.** `pediatr` finds *pediatra*;
`gatto` does not find *gattino*, because *gattino* does not begin with *gatto*.
Nothing matches in the middle of a word, so `casa` never comes back on
*cassata*.

**Capitals and accents make no difference.** `citta` finds *città*, and *città*
finds `citta`.

**Give two words and a page comes back only if it carries both** — in the same
place: in one fact on a standard wiki, or anywhere on the page of a smart one.

This is a search for words, not a question. It does not think about what you
meant, it does not rank one page above another, and it uses no model: pages
come back wiki by wiki, page by page, in name order.

## What is searched, and what is not

**Only what you are allowed to read.** On a **standard** wiki — the memory the
engine writes about people — the search looks at the facts you may read, and a
page appears when one of those facts carries the words. A fact somebody else
wrote for somebody else is not searched and cannot bring back its page. On a
**smart** wiki — the kind a coding assistant keeps for a project — the search
reads the pages themselves, and only in a wiki you may open at all.

On a standard wiki, then, the words are matched against the facts and not
against the sentences the page writer weaves around them: a word that lives
only in that connecting prose does not bring its page back. That is the price
of the rule above — a page is opened by something you are allowed to read, and
nothing else.

So two people searching the same word on the same deployment get two different
answers, and neither of them learns anything about the other's.

Something you said a moment ago may not be found yet. A new fact is remembered
and recallable straight away, but it is filed onto a **page** only at the next
scheduled run, and until it has a page a search over pages cannot bring it
back. In the meantime it is on the [Facts](your-facts.md) page, which lists
what is still waiting. A list or a note you asked the memory to keep is filed
on the spot, and is searchable at once.

One search lists at most 200 pages. When it fills up, the page says so — add
another word to narrow it.

If you are the admin and **Admin reveal** is on, the search widens the same way
every other screen does: every fact and every wiki, whoever they belong to. The
red banner at the top of the results is there to say so.
